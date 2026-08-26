//! Reading cookies out of a Gecko-family `cookies.sqlite` database.
//!
//! Firefox and its forks store cookies in plain text in the `moz_cookies` table — the
//! whole of the work is the read itself. The expiry is Unix seconds, with zero meaning a
//! session cookie.

use rusqlite::Connection;
use std::path::Path;

use super::{Cookie, CookieError, Query};

/// Reads every cookie the query asks for.
pub fn read(database: &Path, query: &Query) -> Result<Vec<Cookie>, CookieError> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
            |source| CookieError::Database {
                path: database.to_path_buf(),
                source,
            },
        )?;
    let mut statement = connection
        .prepare("SELECT host, name, value, path, isSecure, expiry FROM moz_cookies")
        .map_err(|source| CookieError::Database {
            path: database.to_path_buf(),
            source,
        })?;

    let mut cookies = Vec::new();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|source| CookieError::Database {
            path: database.to_path_buf(),
            source,
        })?;
    for row in rows {
        let (host, name, value, path, secure, expiry) =
            row.map_err(|source| CookieError::Database {
                path: database.to_path_buf(),
                source,
            })?;
        if !query.matches(&host, &name) {
            continue;
        }
        cookies.push(Cookie {
            host,
            name,
            value,
            path,
            secure: secure != 0,
            expires_at: expires(expiry),
        });
    }
    Ok(cookies)
}

/// Gecko's `expiry` is Unix seconds; zero means a session cookie.
fn expires(unix: i64) -> Option<tidemark_types::Timestamp> {
    if unix <= 0 {
        return None;
    }
    tidemark_types::Timestamp::from_unix(unix).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tests::TestHome;
    use std::path::PathBuf;

    /// A Gecko cookie database holding exactly the cookies given.
    fn database(home: &TestHome, cookies: &[(&str, &str, &str, i64, i64)]) -> PathBuf {
        let path = home.path().join("cookies.sqlite");
        let connection = Connection::open(&path).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT,
                    value TEXT,
                    host TEXT,
                    path TEXT,
                    expiry INTEGER,
                    lastAccessed INTEGER,
                    creationTime INTEGER,
                    isSecure INTEGER,
                    isHttpOnly INTEGER,
                    inBrowserElement INTEGER DEFAULT 0,
                    sameSite INTEGER DEFAULT 0,
                    rawSameSite INTEGER DEFAULT 0,
                    schemeMap INTEGER DEFAULT 0
                );",
            )
            .expect("creates the table");
        for (host, name, value, expiry, secure) in cookies {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES (?1, ?2, ?3, '/', ?4, ?5, 0, 0, 0)",
                    (host, name, value, expiry, secure),
                )
                .expect("inserts");
        }
        path
    }

    #[test]
    fn a_cookie_is_read_in_plain_text() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[(
                ".cursor.com",
                "WorkosCursorSessionToken",
                "the-session",
                0,
                1,
            )],
        );

        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);
        let cookies = read(&path, &query).expect("reads");

        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].value, "the-session");
        assert!(cookies[0].secure);
        assert_eq!(cookies[0].expires_at, None);
    }

    #[test]
    fn an_expiry_in_unix_seconds_is_carried_through() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[(".example.com", "session", "abc", 1_787_443_200, 0)],
        );

        let query = Query::new(["example.com"], ["session"]);
        let cookies = read(&path, &query).expect("reads");

        assert_eq!(
            cookies[0].expires_at.map(|at| at.as_unix()),
            Some(1_787_443_200)
        );
    }

    #[test]
    fn cookies_the_query_does_not_name_are_never_read() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[
                (
                    ".cursor.com",
                    "WorkosCursorSessionToken",
                    "the-session",
                    0,
                    1,
                ),
                (".google.com", "NID", "not-ours", 0, 1),
            ],
        );

        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);
        let cookies = read(&path, &query).expect("reads");

        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "WorkosCursorSessionToken");
    }
}
