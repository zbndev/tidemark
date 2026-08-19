//! Schema and migrations.
//!
//! Versioned through `PRAGMA user_version`. Every future change appends a step; steps are
//! never edited in place, because installed copies have already run them.

use rusqlite::Connection;

/// The schema version this build expects.
pub const CURRENT_VERSION: i64 = 1;

const V1: &str = r"
CREATE TABLE point (
    provider     TEXT    NOT NULL,
    account      TEXT    NOT NULL,
    window       TEXT    NOT NULL,
    segment      INTEGER NOT NULL,
    captured_at  INTEGER NOT NULL,
    used_percent REAL    NOT NULL,
    resets_at    INTEGER,
    PRIMARY KEY (provider, account, window, segment, captured_at)
) WITHOUT ROWID;

CREATE INDEX point_by_time ON point (captured_at);

CREATE TABLE segment (
    provider    TEXT    NOT NULL,
    account     TEXT    NOT NULL,
    window      TEXT    NOT NULL,
    segment     INTEGER NOT NULL,
    title       TEXT    NOT NULL,
    length_secs INTEGER,
    started_at  INTEGER NOT NULL,
    ended_at    INTEGER,
    resets_at   INTEGER,
    PRIMARY KEY (provider, account, window, segment)
) WITHOUT ROWID;

-- The last reading of every window, written on every poll whether or not a point was
-- stored. Segmentation compares against what was last *observed*, which is not the same as
-- what was last *written*: a window can roll over without consumption moving, and that
-- transition is only visible in a reading nobody kept.
CREATE TABLE window_state (
    provider             TEXT    NOT NULL,
    account              TEXT    NOT NULL,
    window               TEXT    NOT NULL,
    segment              INTEGER NOT NULL,
    last_captured_at     INTEGER NOT NULL,
    last_used_percent    REAL    NOT NULL,
    last_resets_at       INTEGER,
    written_at           INTEGER NOT NULL,
    written_used_percent REAL    NOT NULL,
    PRIMARY KEY (provider, account, window)
) WITHOUT ROWID;
";

/// Brings a connection up to [`CURRENT_VERSION`].
pub fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version < 1 {
        connection.execute_batch(V1)?;
    }

    if version != CURRENT_VERSION {
        connection.pragma_update(None, "user_version", CURRENT_VERSION)?;
    }
    Ok(())
}
