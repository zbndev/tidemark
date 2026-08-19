//! The history database.
//!
//! One SQLite file, keyed `(provider, account, window, segment)` throughout. The account
//! component is present from day one even though the interface shows a single account per
//! provider, so that multi-account is a change to the interface rather than a migration.

pub mod schema;
pub mod segment;

use rusqlite::{Connection, OptionalExtension, params};
use segment::{Boundary, Observation, classify};
use std::path::{Path, PathBuf};
use tidemark_types::{Snapshot, Timestamp, Window, WindowKey};

/// Points are kept at full resolution for this long.
pub const FULL_RESOLUTION_SECS: i64 = 90 * 24 * 3600;

/// Beyond [`FULL_RESOLUTION_SECS`], one point is kept per bucket of this size.
pub const THINNED_BUCKET_SECS: i64 = 15 * 60;

/// A point is written when consumption changes, and otherwise at most this far apart, so
/// that a flat window still leaves a trace to draw.
pub const ANCHOR_INTERVAL_SECS: i64 = 3600;

/// What went wrong reaching the history.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// The database itself refused.
    #[error("history database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The directory holding the database could not be created.
    #[error("could not create {}: {source}", .path.display())]
    Directory {
        /// Directory that could not be created.
        path: PathBuf,
        /// Underlying failure.
        source: std::io::Error,
    },
}

/// One stored reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// When it was taken.
    pub captured_at: Timestamp,
    /// Consumption at that moment.
    pub used_percent: f64,
    /// The reset time known at that moment.
    pub resets_at: Option<Timestamp>,
}

/// What ingesting one window did.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowOutcome {
    /// Which window.
    pub key: WindowKey,
    /// The segment the reading landed in.
    pub segment: i64,
    /// Why a new segment was opened, if one was. `None` means the reading continued the
    /// segment that was already open, or opened the very first one.
    pub boundary: Option<Boundary>,
    /// Whether a point was stored, as opposed to the reading only updating the last-seen
    /// state.
    pub stored: bool,
}

/// What ingesting one snapshot did. Notifications read this to know when a segment turned
/// over, which is what their deduplication is keyed on.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct IngestReport {
    /// One entry per window in the snapshot, in the order they arrived.
    pub windows: Vec<WindowOutcome>,
    /// Windows whose reading was not newer than the one already stored, and was therefore
    /// ignored rather than rewinding the state.
    pub stale: Vec<WindowKey>,
}

impl IngestReport {
    /// Windows that opened a new segment. The unit notification deduplication resets on.
    pub fn segments_opened(&self) -> impl Iterator<Item = &WindowOutcome> {
        self.windows.iter().filter(|w| w.boundary.is_some())
    }
}

/// The history database.
#[derive(Debug)]
pub struct History {
    connection: Connection,
}

impl History {
    /// Opens, creating the file and its directory if needed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StorageError::Directory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Self::prepare(connection)
    }

    /// Opens a throwaway database. Used by tests and by the corpus replay.
    pub fn in_memory() -> Result<Self, StorageError> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(connection: Connection) -> Result<Self, StorageError> {
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        schema::migrate(&connection)?;
        Ok(Self { connection })
    }

    /// Files one poll's worth of readings.
    ///
    /// Readings that are not newer than what is already stored are ignored: a repeated or
    /// out-of-order poll must not rewind the state that segmentation compares against.
    pub fn ingest(&mut self, snapshot: &Snapshot) -> Result<IngestReport, StorageError> {
        let transaction = self.connection.transaction()?;
        let mut report = IngestReport::default();

        for window in &snapshot.windows {
            let observation = Observation {
                captured_at: snapshot.captured_at,
                used_percent: window.used_percent,
                resets_at: window.resets_at,
            };
            let key = (
                snapshot.provider.as_str(),
                snapshot.account.as_str(),
                window.key.as_str(),
            );

            let previous = load_state(&transaction, key)?;

            let (segment, boundary) = match previous {
                None => (1, None),
                Some(ref state) if observation.captured_at <= state.last_captured_at => {
                    report.stale.push(window.key.clone());
                    continue;
                }
                Some(ref state) => match classify(&state.as_observation(), &observation) {
                    b if b.starts_new_segment() => (state.segment + 1, Some(b)),
                    _ => (state.segment, None),
                },
            };

            if let (Some(state), Some(_)) = (previous.as_ref(), boundary) {
                close_segment(&transaction, key, state.segment, state.last_captured_at)?;
            }
            if boundary.is_some() || previous.is_none() {
                open_segment(&transaction, key, segment, window, &observation)?;
            } else {
                touch_segment(&transaction, key, segment, &observation)?;
            }

            let stored = boundary.is_some()
                || match previous.as_ref() {
                    None => true,
                    Some(state) => {
                        state.written_used_percent != observation.used_percent
                            || state.written_at.seconds_until(observation.captured_at)
                                >= ANCHOR_INTERVAL_SECS
                    }
                };

            if stored {
                write_point(&transaction, key, segment, &observation)?;
            }

            let written = if stored {
                (observation.captured_at, observation.used_percent)
            } else {
                previous
                    .as_ref()
                    .map(|s| (s.written_at, s.written_used_percent))
                    .unwrap_or((observation.captured_at, observation.used_percent))
            };
            save_state(&transaction, key, segment, &observation, written)?;

            report.windows.push(WindowOutcome {
                key: window.key.clone(),
                segment,
                boundary,
                stored,
            });
        }

        transaction.commit()?;
        Ok(report)
    }

    /// Thins points older than [`FULL_RESOLUTION_SECS`] down to one per
    /// [`THINNED_BUCKET_SECS`], and returns how many were removed.
    ///
    /// The first and last point of every segment survive regardless. Without that a
    /// thinned segment loses the shape that made it worth keeping: where it started, and
    /// how full it got before it rolled over.
    pub fn thin(&mut self, now: Timestamp) -> Result<usize, StorageError> {
        let cutoff = now.as_unix() - FULL_RESOLUTION_SECS;
        let removed = self.connection.execute(
            r"
            DELETE FROM point
            WHERE captured_at < ?1
              AND EXISTS (
                    SELECT 1 FROM point AS earlier
                    WHERE earlier.provider = point.provider
                      AND earlier.account  = point.account
                      AND earlier.window   = point.window
                      AND earlier.segment  = point.segment
                      AND earlier.captured_at / ?2 = point.captured_at / ?2
                      AND earlier.captured_at < point.captured_at
              )
              AND captured_at <> (
                    SELECT MIN(captured_at) FROM point AS edge
                    WHERE edge.provider = point.provider
                      AND edge.account  = point.account
                      AND edge.window   = point.window
                      AND edge.segment  = point.segment
              )
              AND captured_at <> (
                    SELECT MAX(captured_at) FROM point AS edge
                    WHERE edge.provider = point.provider
                      AND edge.account  = point.account
                      AND edge.window   = point.window
                      AND edge.segment  = point.segment
              )
            ",
            params![cutoff, THINNED_BUCKET_SECS],
        )?;
        Ok(removed)
    }

    /// The segment currently open for a window, if the window has ever been seen.
    pub fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &WindowKey,
    ) -> Result<Option<i64>, StorageError> {
        Ok(self
            .connection
            .query_row(
                "SELECT segment FROM window_state
                 WHERE provider = ?1 AND account = ?2 AND window = ?3",
                params![provider, account, window.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// How many segments a window has accumulated.
    pub fn segment_count(
        &self,
        provider: &str,
        account: &str,
        window: &WindowKey,
    ) -> Result<i64, StorageError> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM segment
             WHERE provider = ?1 AND account = ?2 AND window = ?3",
            params![provider, account, window.as_str()],
            |row| row.get(0),
        )?)
    }

    /// Every point in one segment, oldest first. This is what the burn-down chart draws.
    pub fn points(
        &self,
        provider: &str,
        account: &str,
        window: &WindowKey,
        segment: i64,
    ) -> Result<Vec<Point>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT captured_at, used_percent, resets_at FROM point
             WHERE provider = ?1 AND account = ?2 AND window = ?3 AND segment = ?4
             ORDER BY captured_at",
        )?;
        let rows = statement.query_map(
            params![provider, account, window.as_str(), segment],
            |row| {
                Ok(Point {
                    captured_at: stamp(row.get::<_, i64>(0)?),
                    used_percent: row.get(1)?,
                    resets_at: row.get::<_, Option<i64>>(2)?.map(stamp),
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(StorageError::from)
    }

    /// Total points stored, across everything. Diagnostics and tests.
    pub fn point_count(&self) -> Result<i64, StorageError> {
        Ok(self
            .connection
            .query_row("SELECT COUNT(*) FROM point", [], |row| row.get(0))?)
    }
}

/// Values already in the database have been through [`Timestamp::from_unix`] once, so a
/// value that fails now means the file was edited by hand; clamping is a better answer
/// than refusing to open the history.
fn stamp(seconds: i64) -> Timestamp {
    Timestamp::from_unix(seconds.clamp(Timestamp::EARLIEST, Timestamp::LATEST - 1))
        .unwrap_or_else(|_| Timestamp::now())
}

#[derive(Debug)]
struct State {
    segment: i64,
    last_captured_at: Timestamp,
    last_used_percent: f64,
    last_resets_at: Option<Timestamp>,
    written_at: Timestamp,
    written_used_percent: f64,
}

impl State {
    fn as_observation(&self) -> Observation {
        Observation {
            captured_at: self.last_captured_at,
            used_percent: self.last_used_percent,
            resets_at: self.last_resets_at,
        }
    }
}

type Key<'a> = (&'a str, &'a str, &'a str);

fn load_state(
    connection: &Connection,
    (provider, account, window): Key<'_>,
) -> Result<Option<State>, StorageError> {
    Ok(connection
        .query_row(
            "SELECT segment, last_captured_at, last_used_percent, last_resets_at,
                    written_at, written_used_percent
             FROM window_state WHERE provider = ?1 AND account = ?2 AND window = ?3",
            params![provider, account, window],
            |row| {
                Ok(State {
                    segment: row.get(0)?,
                    last_captured_at: stamp(row.get::<_, i64>(1)?),
                    last_used_percent: row.get(2)?,
                    last_resets_at: row.get::<_, Option<i64>>(3)?.map(stamp),
                    written_at: stamp(row.get::<_, i64>(4)?),
                    written_used_percent: row.get(5)?,
                })
            },
        )
        .optional()?)
}

fn save_state(
    connection: &Connection,
    (provider, account, window): Key<'_>,
    segment: i64,
    observation: &Observation,
    (written_at, written_used_percent): (Timestamp, f64),
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT INTO window_state
             (provider, account, window, segment, last_captured_at, last_used_percent,
              last_resets_at, written_at, written_used_percent)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (provider, account, window) DO UPDATE SET
             segment = excluded.segment,
             last_captured_at = excluded.last_captured_at,
             last_used_percent = excluded.last_used_percent,
             last_resets_at = excluded.last_resets_at,
             written_at = excluded.written_at,
             written_used_percent = excluded.written_used_percent",
        params![
            provider,
            account,
            window,
            segment,
            observation.captured_at.as_unix(),
            observation.used_percent,
            observation.resets_at.map(Timestamp::as_unix),
            written_at.as_unix(),
            written_used_percent,
        ],
    )?;
    Ok(())
}

fn open_segment(
    connection: &Connection,
    (provider, account, window): Key<'_>,
    segment: i64,
    definition: &Window,
    observation: &Observation,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT OR REPLACE INTO segment
             (provider, account, window, segment, title, length_secs, started_at,
              ended_at, resets_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8)",
        params![
            provider,
            account,
            window,
            segment,
            definition.title,
            definition.length.map(|l| l.as_secs() as i64),
            observation.captured_at.as_unix(),
            observation.resets_at.map(Timestamp::as_unix),
        ],
    )?;
    Ok(())
}

fn touch_segment(
    connection: &Connection,
    (provider, account, window): Key<'_>,
    segment: i64,
    observation: &Observation,
) -> Result<(), StorageError> {
    connection.execute(
        "UPDATE segment SET resets_at = ?5
         WHERE provider = ?1 AND account = ?2 AND window = ?3 AND segment = ?4",
        params![
            provider,
            account,
            window,
            segment,
            observation.resets_at.map(Timestamp::as_unix)
        ],
    )?;
    Ok(())
}

fn close_segment(
    connection: &Connection,
    (provider, account, window): Key<'_>,
    segment: i64,
    ended_at: Timestamp,
) -> Result<(), StorageError> {
    connection.execute(
        "UPDATE segment SET ended_at = ?5
         WHERE provider = ?1 AND account = ?2 AND window = ?3 AND segment = ?4",
        params![provider, account, window, segment, ended_at.as_unix()],
    )?;
    Ok(())
}

fn write_point(
    connection: &Connection,
    (provider, account, window): Key<'_>,
    segment: i64,
    observation: &Observation,
) -> Result<(), StorageError> {
    connection.execute(
        "INSERT OR REPLACE INTO point
             (provider, account, window, segment, captured_at, used_percent, resets_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            provider,
            account,
            window,
            segment,
            observation.captured_at.as_unix(),
            observation.used_percent,
            observation.resets_at.map(Timestamp::as_unix),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, WindowLength};

    const HOUR: i64 = 3600;
    const POLL: i64 = 300;

    fn at(offset: i64) -> Timestamp {
        Timestamp::from_unix(1_785_700_000 + offset).expect("plausible")
    }

    fn key() -> WindowKey {
        WindowKey::for_length(WindowLength::from_secs(5 * 3600).expect("nonzero"))
    }

    fn snapshot(captured: i64, used: f64, resets: Option<i64>) -> Snapshot {
        Snapshot {
            provider: ProviderId::new("test"),
            account: AccountId::default(),
            captured_at: at(captured),
            windows: vec![Window {
                key: key(),
                title: "five hours".into(),
                used_percent: used,
                resets_at: resets.map(at),
                length: WindowLength::from_secs(5 * 3600),
            }],
            details: Vec::new(),
        }
    }

    fn history() -> History {
        History::in_memory().expect("in-memory database")
    }

    fn segments(history: &History) -> i64 {
        history
            .segment_count("test", "default", &key())
            .expect("counted")
    }

    #[test]
    fn the_first_reading_opens_segment_one_and_is_stored() {
        let mut history = history();
        let report = history
            .ingest(&snapshot(0, 10.0, Some(5 * HOUR)))
            .expect("ingested");
        assert_eq!(report.windows[0].segment, 1);
        assert_eq!(report.windows[0].boundary, None);
        assert!(report.windows[0].stored);
        assert_eq!(history.point_count().expect("counted"), 1);
    }

    #[test]
    fn an_unchanged_reading_updates_state_without_storing_a_point() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 10.0, Some(5 * HOUR)))
            .expect("ingested");
        let report = history
            .ingest(&snapshot(POLL, 10.0, Some(5 * HOUR)))
            .expect("ingested");
        assert!(!report.windows[0].stored);
        assert_eq!(history.point_count().expect("counted"), 1);
    }

    #[test]
    fn a_flat_window_still_leaves_an_hourly_anchor() {
        let mut history = history();
        for step in 0..25 {
            history
                .ingest(&snapshot(step * POLL, 10.0, Some(5 * HOUR)))
                .expect("ingested");
        }
        // Twenty-five polls five minutes apart span exactly two hours: the opening point,
        // then one anchor per elapsed hour. The other twenty-two readings changed nothing
        // and are not worth a row.
        assert_eq!(history.point_count().expect("counted"), 3);
    }

    #[test]
    fn a_drifting_reset_time_never_opens_a_second_segment() {
        let mut history = history();
        for step in 0..500 {
            history
                .ingest(&snapshot(
                    step * POLL,
                    12.0,
                    Some(7 * 24 * HOUR + step * POLL),
                ))
                .expect("ingested");
        }
        assert_eq!(segments(&history), 1);
    }

    #[test]
    fn a_rollover_opens_a_segment_and_closes_the_previous_one() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 96.0, Some(HOUR)))
            .expect("ingested");
        let report = history
            .ingest(&snapshot(POLL, 2.0, Some(HOUR + 5 * HOUR)))
            .expect("ingested");
        assert_eq!(report.windows[0].boundary, Some(Boundary::UsageDropped));
        assert_eq!(report.windows[0].segment, 2);
        assert_eq!(segments(&history), 2);
        assert_eq!(report.segments_opened().count(), 1);
    }

    #[test]
    fn a_repeated_poll_is_ignored_rather_than_rewinding_the_state() {
        let mut history = history();
        history
            .ingest(&snapshot(POLL, 40.0, Some(5 * HOUR)))
            .expect("ingested");
        let report = history
            .ingest(&snapshot(0, 5.0, Some(5 * HOUR)))
            .expect("ingested");
        assert_eq!(report.stale, vec![key()]);
        assert!(report.windows.is_empty());
        assert_eq!(segments(&history), 1);
        assert_eq!(history.point_count().expect("counted"), 1);
    }

    #[test]
    fn a_window_the_provider_stopped_reporting_leaves_its_history_alone() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 40.0, Some(5 * HOUR)))
            .expect("ingested");
        let mut empty = snapshot(POLL, 0.0, None);
        empty.windows.clear();
        let report = history.ingest(&empty).expect("ingested");
        assert!(report.windows.is_empty());
        assert_eq!(segments(&history), 1);
        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("looked up"),
            Some(1)
        );
    }

    #[test]
    fn points_come_back_in_order_with_their_reset_times() {
        let mut history = history();
        for step in 0..4 {
            history
                .ingest(&snapshot(step * POLL, step as f64, Some(5 * HOUR)))
                .expect("ingested");
        }
        let points = history.points("test", "default", &key(), 1).expect("read");
        assert_eq!(points.len(), 4);
        assert!(
            points
                .windows(2)
                .all(|w| w[0].captured_at < w[1].captured_at)
        );
        assert_eq!(points[3].used_percent, 3.0);
        assert_eq!(points[0].resets_at, Some(at(5 * HOUR)));
    }

    #[test]
    fn thinning_leaves_recent_history_untouched() {
        let mut history = history();
        for step in 0..40 {
            history
                .ingest(&snapshot(step * POLL, step as f64, Some(5 * HOUR)))
                .expect("ingested");
        }
        let before = history.point_count().expect("counted");
        let removed = history.thin(at(40 * POLL)).expect("thinned");
        assert_eq!(removed, 0);
        assert_eq!(history.point_count().expect("counted"), before);
    }

    #[test]
    fn thinning_old_history_keeps_one_point_per_bucket_and_both_edges() {
        let mut history = history();
        let count = 40;
        for step in 0..count {
            history
                .ingest(&snapshot(step * 60, step as f64, Some(5 * HOUR)))
                .expect("ingested");
        }
        let now = at(count * 60 + FULL_RESOLUTION_SECS + 1);
        assert!(history.thin(now).expect("thinned") > 0);

        let points = history.points("test", "default", &key(), 1).expect("read");
        // Forty minutes at one-minute resolution collapses to one point per quarter hour,
        // and the last point survives as the segment's edge.
        assert_eq!(points.len(), 4);
        assert_eq!(points[0].used_percent, 0.0);
        assert_eq!(points[points.len() - 1].used_percent, (count - 1) as f64);
        assert_eq!(history.thin(now).expect("thinned again"), 0);
    }

    #[test]
    fn accounts_of_the_same_provider_do_not_share_a_segment_counter() {
        let mut history = history();
        let mut first = snapshot(0, 90.0, Some(HOUR));
        first.account = AccountId::new("one");
        let mut second = snapshot(0, 4.0, Some(HOUR));
        second.account = AccountId::new("two");
        history.ingest(&first).expect("ingested");
        history.ingest(&second).expect("ingested");

        let mut rolled = snapshot(POLL, 1.0, Some(HOUR + 5 * HOUR));
        rolled.account = AccountId::new("one");
        history.ingest(&rolled).expect("ingested");

        assert_eq!(
            history
                .current_segment("test", "one", &key())
                .expect("looked up"),
            Some(2)
        );
        assert_eq!(
            history
                .current_segment("test", "two", &key())
                .expect("looked up"),
            Some(1)
        );
    }

    #[test]
    fn a_reopened_database_continues_the_segment_it_left_open() {
        let directory = std::env::temp_dir().join(format!("tidemark-test-{}", std::process::id()));
        let path = directory.join("history.db");
        let _ = std::fs::remove_dir_all(&directory);

        {
            let mut history = History::open(&path).expect("opened");
            history
                .ingest(&snapshot(0, 96.0, Some(HOUR)))
                .expect("ingested");
            history
                .ingest(&snapshot(POLL, 2.0, Some(HOUR + 5 * HOUR)))
                .expect("ingested");
        }
        {
            let mut history = History::open(&path).expect("reopened");
            let report = history
                .ingest(&snapshot(2 * POLL, 3.0, Some(HOUR + 5 * HOUR)))
                .expect("ingested");
            assert_eq!(report.windows[0].segment, 2);
            assert_eq!(report.windows[0].boundary, None);
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
}
