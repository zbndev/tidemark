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
    /// Rekeying would overwrite rows already owned by the destination account.
    #[error("history account rekey destination is not empty: {provider}/{account}")]
    AccountRekeyCollision {
        /// Provider whose account rows would collide.
        provider: String,
        /// Destination account id containing existing rows.
        account: String,
    },
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

        // Notices are state, not history: once the segment they deduplicate against has
        // been closed for longer than anything is kept at full resolution, no reading can
        // ever land in it again and the row can only grow the file.
        self.connection.execute(
            r"
            DELETE FROM notice
            WHERE EXISTS (
                    SELECT 1 FROM segment
                    WHERE segment.provider = notice.provider
                      AND segment.account  = notice.account
                      AND segment.window   = notice.window
                      AND segment.segment  = notice.segment
                      AND segment.ended_at IS NOT NULL
                      AND segment.ended_at < ?1
            )
            ",
            params![cutoff],
        )?;

        Ok(removed)
    }

    /// Deletes history older than an explicit retention cutoff.
    ///
    /// An open segment and its latest observed state survive even when the daemon has been
    /// offline longer than the retention period. The next reading still needs that state
    /// to decide whether a reset happened while it was away.
    pub fn prune_before(&mut self, cutoff: Timestamp) -> Result<usize, StorageError> {
        let transaction = self.connection.transaction()?;
        let removed = transaction.execute(
            "DELETE FROM point WHERE captured_at < ?1",
            params![cutoff.as_unix()],
        )?;
        transaction.execute(
            r"
            DELETE FROM notice
            WHERE EXISTS (
                SELECT 1 FROM segment
                WHERE segment.provider = notice.provider
                  AND segment.account  = notice.account
                  AND segment.window   = notice.window
                  AND segment.segment  = notice.segment
                  AND segment.ended_at IS NOT NULL
                  AND segment.ended_at < ?1
            )
            ",
            params![cutoff.as_unix()],
        )?;
        transaction.execute(
            "DELETE FROM segment WHERE ended_at IS NOT NULL AND ended_at < ?1",
            params![cutoff.as_unix()],
        )?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Removes every stored reading, segment, notice and last-observed window state.
    ///
    /// The deletes commit as one transaction: a database that refuses halfway keeps
    /// everything, so the caller can retry instead of inheriting a half-cleared history.
    /// Compaction runs only after the commit, and its failure is logged rather than
    /// reported — the deletion is already durable, and a failed VACUUM is not a failed
    /// clear.
    pub fn clear(&mut self) -> Result<(), StorageError> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            r"
            DELETE FROM notice;
            DELETE FROM point;
            DELETE FROM window_state;
            DELETE FROM segment;
            ",
        )?;
        transaction.commit()?;
        if let Err(error) = self.connection.execute_batch("VACUUM") {
            tracing::warn!(%error, "history cleared but could not be compacted");
        }
        Ok(())
    }

    /// Whether a notification of this kind has already gone out for this segment.
    ///
    /// Kinds are the daemon's vocabulary rather than the database's: what is stored is
    /// whatever string the caller deduplicates on.
    pub fn notice_sent(
        &self,
        provider: &str,
        account: &str,
        window: &WindowKey,
        segment: i64,
        kind: &str,
    ) -> Result<bool, StorageError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM notice
                 WHERE provider = ?1 AND account = ?2 AND window = ?3
                   AND segment = ?4 AND kind = ?5",
                params![provider, account, window.as_str(), segment, kind],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Records that a notification of this kind went out for this segment.
    ///
    /// Written **after** the notification server accepted it, never before: a row written
    /// ahead of a delivery that then failed is a warning the user never sees and the
    /// daemon never retries. Repeating a record is not an error — the first time stands.
    pub fn record_notice(
        &mut self,
        provider: &str,
        account: &str,
        window: &WindowKey,
        segment: i64,
        kind: &str,
        sent_at: Timestamp,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT OR IGNORE INTO notice
                 (provider, account, window, segment, kind, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                provider,
                account,
                window.as_str(),
                segment,
                kind,
                sent_at.as_unix()
            ],
        )?;
        Ok(())
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

    /// Every stored point in the segment that is currently open for a window.
    ///
    /// A window the daemon has not seen yet has no current segment, which is an ordinary
    /// empty chart rather than a storage error.
    pub fn current_points(
        &self,
        provider: &str,
        account: &str,
        window: &WindowKey,
    ) -> Result<Vec<Point>, StorageError> {
        let Some(segment) = self.current_segment(provider, account, window)? else {
            return Ok(Vec::new());
        };
        self.points(provider, account, window, segment)
    }
    /// Checks that moving rows to `new` cannot overwrite another account's history.
    ///
    /// History has no merge semantics: a destination with any row is rejected so a
    /// promotion cannot silently discard windows, points, or notifications.
    pub fn can_rekey_account(
        &self,
        provider: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        if old == new {
            return Ok(());
        }
        for table in ["window_state", "segment", "point", "notice"] {
            let occupied: i64 = self.connection.query_row(
                &format!(
                    "SELECT EXISTS(
                        SELECT 1 FROM {table}
                        WHERE provider = ?1 AND account = ?2
                    )"
                ),
                params![provider, new],
                |row| row.get(0),
            )?;
            if occupied != 0 {
                return Err(StorageError::AccountRekeyCollision {
                    provider: provider.to_owned(),
                    account: new.to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Moves every stored row for one provider/account pair to another account id,
    /// refusing when the destination already holds anything.
    ///
    /// History has no merge semantics and this is the caller that must not clobber: a
    /// destination with any row is rejected up front by [`Self::can_rekey_account`] so a
    /// move cannot silently discard windows, points, or notifications. The updates run
    /// in one transaction so a failure in any table leaves all four tables unchanged.
    /// Account rename and promotion do not land here: their destination ids are
    /// unconfigured or retired by construction, so they use
    /// [`Self::rekey_account_discarding_destination`] instead.
    pub fn rekey_account(
        &mut self,
        provider: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        self.can_rekey_account(provider, old, new)?;
        Self::move_account_rows(self.connection.transaction()?, provider, old, new, false)
    }

    /// Moves every stored row for one provider/account pair to another account id,
    /// discarding whatever the destination still holds in the same transaction.
    ///
    /// The destination of an account rename or a promotion was validated unconfigured, or
    /// is the id of an account the very same flow removes, so any row already under it
    /// belongs to an id nothing will answer for: keeping it would attribute a
    /// predecessor's usage to the account that inherits the id, which is the cross-account
    /// pollution this storage exists to prevent. Clearing and moving in one transaction
    /// leaves either all of the old id's rows in place and the destination cleared, or
    /// neither.
    pub fn rekey_account_discarding_destination(
        &mut self,
        provider: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        if old == new {
            // Clearing the destination would clear the source with it; there is no move
            // to make.
            return Ok(());
        }
        Self::move_account_rows(self.connection.transaction()?, provider, old, new, true)
    }

    /// The statements behind both rekey variants, run inside one transaction so the
    /// destination clearing and the move commit or roll back together.
    fn move_account_rows(
        transaction: rusqlite::Transaction<'_>,
        provider: &str,
        old: &str,
        new: &str,
        clear_destination: bool,
    ) -> Result<(), StorageError> {
        for table in ["window_state", "segment", "point", "notice"] {
            if clear_destination {
                transaction.execute(
                    &format!("DELETE FROM {table} WHERE provider = ?1 AND account = ?2"),
                    params![provider, new],
                )?;
            }
            transaction.execute(
                &format!(
                    "UPDATE {table} SET account = ?1
                     WHERE provider = ?2 AND account = ?3"
                ),
                params![new, provider, old],
            )?;
        }
        transaction.commit()?;
        Ok(())
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
                subtitle: None,
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
    fn current_points_exclude_the_segment_before_a_rollover() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 10.0, Some(5 * HOUR)))
            .expect("first segment starts");
        history
            .ingest(&snapshot(POLL, 80.0, Some(5 * HOUR)))
            .expect("first segment advances");
        history
            .ingest(&snapshot(2 * POLL, 2.0, Some(10 * HOUR)))
            .expect("rolls over");
        history
            .ingest(&snapshot(3 * POLL, 20.0, Some(10 * HOUR)))
            .expect("current segment advances");

        let points = history
            .current_points("test", "default", &key())
            .expect("current segment reads");
        assert_eq!(
            points
                .iter()
                .map(|point| point.used_percent)
                .collect::<Vec<_>>(),
            [2.0, 20.0]
        );
    }

    #[test]
    fn current_points_are_empty_for_a_window_never_seen() {
        let history = history();
        assert!(
            history
                .current_points("test", "default", &key())
                .expect("unseen history reads")
                .is_empty()
        );
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
    fn rekeying_moves_all_history_rows_to_the_new_account() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested");
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");

        history
            .rekey_account("test", "default", "work")
            .expect("rekeyed");

        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("old state read"),
            None
        );
        assert_eq!(
            history
                .current_segment("test", "work", &key())
                .expect("new state read"),
            Some(1)
        );
        assert!(
            history
                .points("test", "default", &key(), 1)
                .expect("old points read")
                .is_empty()
        );
        assert_eq!(
            history
                .points("test", "work", &key(), 1)
                .expect("new points read")
                .len(),
            1
        );
        assert!(
            !history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("old notice read")
        );
        assert!(
            history
                .notice_sent("test", "work", &key(), 1, "threshold-70")
                .expect("new notice read")
        );
    }

    #[test]
    fn a_rekey_rejects_a_nonempty_destination_without_merging_rows() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested old account");
        let mut destination = snapshot(POLL, 7.0, Some(5 * HOUR));
        destination.account = AccountId::new("work");
        history
            .ingest(&destination)
            .expect("ingested destination account");

        assert!(matches!(
            history.rekey_account("test", "default", "work"),
            Err(StorageError::AccountRekeyCollision { provider, account })
                if provider == "test" && account == "work"
        ));
        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("old state read"),
            Some(1)
        );
        assert_eq!(
            history
                .current_segment("test", "work", &key())
                .expect("destination state read"),
            Some(1)
        );
        assert_eq!(
            history
                .points("test", "default", &key(), 1)
                .expect("old points read")
                .len(),
            1
        );
        assert_eq!(
            history
                .points("test", "work", &key(), 1)
                .expect("destination points read")
                .len(),
            1
        );
    }

    #[test]
    fn a_discarding_rekey_clears_the_stale_destination_rows_and_moves_its_own_in() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested the migrating account");
        let mut stale = snapshot(POLL, 7.0, Some(5 * HOUR));
        stale.account = AccountId::new("work");
        history
            .ingest(&stale)
            .expect("ingested the stale destination");

        history
            .rekey_account_discarding_destination("test", "default", "work")
            .expect("rekeyed");

        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("old state read"),
            None
        );
        assert_eq!(
            history
                .current_segment("test", "work", &key())
                .expect("new state read"),
            Some(1)
        );
        let points = history
            .points("test", "work", &key(), 1)
            .expect("new points read");
        assert_eq!(points.len(), 1);
        assert_eq!(
            points[0].used_percent, 42.0,
            "the account that inherited the id owns the rows under it, not the id's \n             predecessor"
        );
    }

    #[test]
    fn a_discarding_rekey_of_an_id_onto_itself_keeps_every_row() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested");

        history
            .rekey_account_discarding_destination("test", "default", "default")
            .expect("rekeyed");

        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("state read"),
            Some(1)
        );
        assert_eq!(
            history
                .points("test", "default", &key(), 1)
                .expect("points read")
                .len(),
            1,
            "clearing the destination of a no-op move must not clear the source with it"
        );
    }

    #[test]
    fn a_failed_discarding_rekey_leaves_the_destination_rows_in_place() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested the migrating account");
        let mut stale = snapshot(POLL, 7.0, Some(5 * HOUR));
        stale.account = AccountId::new("work");
        history
            .ingest(&stale)
            .expect("ingested the stale destination");
        history
            .connection
            .execute_batch(
                "CREATE TRIGGER pinned_segment BEFORE UPDATE OF account ON segment
                 WHEN NEW.account = 'work'
                 BEGIN SELECT RAISE(FAIL, 'segments are pinned'); END;",
            )
            .expect("trigger installed");

        assert!(
            history
                .rekey_account_discarding_destination("test", "default", "work")
                .is_err(),
            "a pinned segment refuses the rekey"
        );
        assert_eq!(
            history
                .current_segment("test", "work", &key())
                .expect("destination state read"),
            Some(1)
        );
        assert_eq!(
            history
                .points("test", "work", &key(), 1)
                .expect("destination points read")
                .len(),
            1,
            "the stale rows come back with the transaction that failed to move the rest"
        );
    }

    #[test]
    fn a_failed_rekey_keeps_rows_under_the_old_account() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 42.0, Some(5 * HOUR)))
            .expect("ingested");
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");
        history
            .connection
            .execute_batch(
                "CREATE TRIGGER pinned_segment BEFORE UPDATE OF account ON segment
                 WHEN NEW.account = 'work'
                 BEGIN SELECT RAISE(FAIL, 'segments are pinned'); END;",
            )
            .expect("trigger installed");

        assert!(
            history.rekey_account("test", "default", "work").is_err(),
            "a pinned segment refuses the rekey"
        );

        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("old state read"),
            Some(1)
        );
        assert!(
            history
                .current_segment("test", "work", &key())
                .expect("new state read")
                .is_none()
        );
        assert_eq!(
            history
                .points("test", "default", &key(), 1)
                .expect("old points read")
                .len(),
            1
        );
        assert!(
            history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("old notice read")
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

    #[test]
    fn a_notice_nobody_recorded_has_not_been_sent() {
        let history = history();
        assert!(
            !history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("looked up")
        );
    }

    #[test]
    fn a_recorded_notice_reads_back_as_sent() {
        let mut history = history();
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");
        assert!(
            history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("looked up")
        );
    }

    #[test]
    fn one_kind_of_notice_says_nothing_about_another() {
        let mut history = history();
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");
        assert!(
            !history
                .notice_sent("test", "default", &key(), 1, "threshold-90")
                .expect("looked up")
        );
    }

    #[test]
    fn the_next_segment_arms_the_same_notice_again() {
        let mut history = history();
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");
        assert!(
            !history
                .notice_sent("test", "default", &key(), 2, "threshold-70")
                .expect("looked up")
        );
    }

    #[test]
    fn recording_the_same_notice_twice_is_not_an_error() {
        let mut history = history();
        history
            .record_notice("test", "default", &key(), 1, "reset", at(0))
            .expect("recorded");
        history
            .record_notice("test", "default", &key(), 1, "reset", at(POLL))
            .expect("recorded again");
        assert!(
            history
                .notice_sent("test", "default", &key(), 1, "reset")
                .expect("looked up")
        );
    }

    /// The point of filing notices in the database rather than in the daemon's memory: a
    /// restart must not fire the eighty-percent warning at somebody a second time.
    #[test]
    fn a_recorded_notice_survives_reopening_the_database() {
        let directory =
            std::env::temp_dir().join(format!("tidemark-notice-{}", std::process::id()));
        let path = directory.join("history.db");
        let _ = std::fs::remove_dir_all(&directory);

        {
            let mut history = History::open(&path).expect("opened");
            history
                .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
                .expect("recorded");
        }
        {
            let history = History::open(&path).expect("reopened");
            assert!(
                history
                    .notice_sent("test", "default", &key(), 1, "threshold-70")
                    .expect("looked up")
            );
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn thinning_forgets_notices_of_segments_that_closed_long_ago() {
        let mut history = history();
        // Segment one opens, then rolls over, which closes it at the second reading.
        history
            .ingest(&snapshot(0, 96.0, Some(HOUR)))
            .expect("ingested");
        history
            .ingest(&snapshot(POLL, 1.0, Some(HOUR + 5 * HOUR)))
            .expect("ingested");
        history
            .record_notice("test", "default", &key(), 1, "threshold-90", at(0))
            .expect("recorded");
        history
            .record_notice("test", "default", &key(), 2, "reset", at(POLL))
            .expect("recorded");

        history
            .thin(at(POLL + FULL_RESOLUTION_SECS + 1))
            .expect("thinned");

        assert!(
            !history
                .notice_sent("test", "default", &key(), 1, "threshold-90")
                .expect("looked up"),
            "a notice for a segment that closed ninety days ago is state nobody can use"
        );
        assert!(
            history
                .notice_sent("test", "default", &key(), 2, "reset")
                .expect("looked up"),
            "the segment still open must keep its notices"
        );
    }
    #[test]
    fn pruning_drops_points_before_the_retention_cutoff() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 10.0, Some(5 * HOUR)))
            .expect("old point");
        history
            .ingest(&snapshot(2 * HOUR, 20.0, Some(5 * HOUR)))
            .expect("new point");

        assert_eq!(history.prune_before(at(HOUR)).expect("pruned"), 1);
        let points = history.points("test", "default", &key(), 1).expect("read");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].used_percent, 20.0);
    }

    #[test]
    fn clearing_history_removes_points_segments_state_and_notices() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 72.0, Some(5 * HOUR)))
            .expect("ingested");
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");

        history.clear().expect("cleared");

        assert_eq!(history.point_count().expect("points counted"), 0);
        assert_eq!(
            history
                .segment_count("test", "default", &key())
                .expect("segments counted"),
            0
        );
        assert!(
            !history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("notice checked")
        );
        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("state checked"),
            None
        );
    }

    /// A clear the database refuses partway through must keep everything: the caller will
    /// retry, and a half-cleared database would forget notices that still apply while the
    /// segments they belong to survive.
    #[test]
    fn a_clear_that_fails_partway_keeps_everything_stored() {
        let mut history = history();
        history
            .ingest(&snapshot(0, 72.0, Some(5 * HOUR)))
            .expect("ingested");
        history
            .record_notice("test", "default", &key(), 1, "threshold-70", at(0))
            .expect("recorded");
        history
            .connection
            .execute_batch(
                "CREATE TRIGGER pinned_segment BEFORE DELETE ON segment
                 BEGIN SELECT RAISE(FAIL, 'segments are pinned'); END;",
            )
            .expect("trigger installed");

        assert!(
            history.clear().is_err(),
            "a pinned segment refuses the clear"
        );

        assert_eq!(history.point_count().expect("points counted"), 1);
        assert_eq!(
            history
                .segment_count("test", "default", &key())
                .expect("segments counted"),
            1
        );
        assert!(
            history
                .notice_sent("test", "default", &key(), 1, "threshold-70")
                .expect("notice checked"),
            "a notice whose segment survived the failed clear must survive too"
        );
        assert_eq!(
            history
                .current_segment("test", "default", &key())
                .expect("state checked"),
            Some(1)
        );
    }
}
