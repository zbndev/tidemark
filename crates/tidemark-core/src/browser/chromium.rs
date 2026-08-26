//! Reading cookies out of a Chromium-family `Cookies` database.
//!
//! # `os_crypt`, in the order Chromium built it
//!
//! Every cookie value in `cookies.encrypted_value` is sealed with AES-128-CBC and the IV
//! is sixteen spaces. What changed over Chromium's life is where the key comes from, and
//! the first three bytes of the blob say which:
//!
//! - `v10` — the key is derived from the fixed, well-known password `peanuts`, the
//!   fallback Chromium itself uses when no keyring answers.
//! - `v11` — the key is derived from the browser's "Safe Storage" password, a random
//!   value Chromium stores in the Secret Service under its `application` attribute.
//!
//! Either way the derivation is PBKDF2-HMAC-SHA1, salt `saltysalt`, one iteration, sixteen
//! bytes out.
//!
//! # Domain binding
//!
//! Since M130 Chromium prepends `SHA-256(host_key)` to the plaintext before sealing, so a
//! cookie can only be read where it was set. We strip that prefix back off — and a
//! decryptable cookie whose value is not text is treated as unreadable rather than as a
//! value, because UTF-8 is how we tell a right key from a wrong one: a wrong key decrypts
//! to noise, not to an error.

use aes::Aes128;
use cbc::Decryptor;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use rusqlite::Connection;
use std::path::Path;

use super::{Cookie, CookieError, Query};

/// The password `v10` values are sealed with, when no keyring exists.
const BASIC_PASSWORD: &[u8] = b"peanuts";

/// The salt and iteration count Chromium derives both key variants with.
const SALT: &[u8] = b"saltysalt";
const ITERATIONS: u32 = 1;

/// The sixteen-space IV every os_crypt value is sealed with.
const IV: [u8; 16] = [b' '; 16];

/// The microseconds between Chromium's epoch (1601-01-01) and the Unix epoch.
const WEBKIT_EPOCH_OFFSET_MICROS: i64 = 11_644_473_600_000_000;

/// Reads every cookie the query asks for. `password` is the Safe Storage password from the
/// Secret Service, or `None` when the browser never stored one — which is the world `v10`
/// exists for, and also the shape of a machine that was never signed in anywhere.
pub fn read(
    database: &Path,
    query: &Query,
    password: Option<&str>,
) -> Result<Vec<Cookie>, CookieError> {
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
            |source| CookieError::Database {
                path: database.to_path_buf(),
                source,
            },
        )?;
    let mut statement = connection
        .prepare(
            "SELECT host_key, name, encrypted_value, value, path, is_secure, expires_utc \
             FROM cookies",
        )
        .map_err(|source| CookieError::Database {
            path: database.to_path_buf(),
            source,
        })?;

    let key = password.map(|password| derive_key(password.as_bytes()));
    let basic_key = derive_key(BASIC_PASSWORD);

    let mut cookies = Vec::new();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(|source| CookieError::Database {
            path: database.to_path_buf(),
            source,
        })?;
    for row in rows {
        let (host, name, encrypted, plain, path, secure, expires_utc) =
            row.map_err(|source| CookieError::Database {
                path: database.to_path_buf(),
                source,
            })?;
        if !query.matches(&host, &name) {
            continue;
        }
        let Some(value) = value(&encrypted, &plain, &host, key.as_ref(), &basic_key) else {
            continue;
        };
        cookies.push(Cookie {
            host,
            name,
            value,
            path,
            secure: secure != 0,
            expires_at: expires(expires_utc),
        });
    }
    Ok(cookies)
}

/// The cookie's value: the plain-text column when Chromium stored it there, otherwise the
/// decryption of the sealed one. `None` for a value this build cannot read — the wrong
/// keyring password, a sealing version Chromium invents next — rather than garbage on the
/// wire.
fn value(
    encrypted: &[u8],
    plain: &str,
    host: &str,
    key: Option<&[u8; 16]>,
    basic_key: &[u8; 16],
) -> Option<String> {
    if encrypted.is_empty() {
        return Some(plain.to_owned());
    }
    let key = match encrypted {
        [b'v', b'1', b'0', ..] => Some(basic_key),
        [b'v', b'1', b'1', ..] => key,
        _ => None,
    }?;
    let mut body = encrypted[3..].to_vec();
    let padded = Decryptor::<Aes128>::new(key.into(), &IV.into())
        .decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut body)
        .ok()?;
    // M130 and newer seal `SHA-256(host) || value`; the prefix is part of the plaintext,
    // not of the value.
    let digest = {
        use sha2::Digest;
        sha2::Sha256::digest(host.as_bytes())
    };
    let bytes = match padded.strip_prefix(digest.as_slice()) {
        Some(value) => value,
        None => padded,
    };
    String::from_utf8(bytes.to_vec()).ok()
}

/// Chromium's `expires_utc` is microseconds since 1601; zero means a session cookie.
fn expires(webkit_micros: i64) -> Option<tidemark_types::Timestamp> {
    if webkit_micros <= 0 {
        return None;
    }
    let unix = (webkit_micros - WEBKIT_EPOCH_OFFSET_MICROS) / 1_000_000;
    tidemark_types::Timestamp::from_unix(unix).ok()
}

/// The AES key for an os_crypt password.
fn derive_key(password: &[u8]) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, SALT, ITERATIONS, &mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tests::TestHome;
    use cbc::cipher::BlockEncryptMut;
    use std::path::PathBuf;

    /// The sealed form of a value, as Chromium would write it.
    fn seal(password: &[u8], version: &[u8; 3], plaintext: &[u8]) -> Vec<u8> {
        let key = derive_key(password);
        let mut body = Vec::new();
        body.extend_from_slice(version);
        let length = plaintext.len() + (16 - plaintext.len() % 16);
        let mut buffer = vec![0u8; length];
        buffer[..plaintext.len()].copy_from_slice(plaintext);
        let sealed = cbc::Encryptor::<Aes128>::new((&key).into(), &IV.into())
            .encrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buffer, plaintext.len())
            .expect("seals")
            .to_vec();
        body.extend_from_slice(&sealed);
        body
    }

    /// A Chromium cookie database holding exactly the cookies given.
    fn database(home: &TestHome, cookies: &[(&str, &str, Vec<u8>, &str, i64)]) -> PathBuf {
        let path = home.path().join("Cookies");
        let connection = Connection::open(&path).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE cookies (
                    creation_utc INTEGER NOT NULL,
                    host_key TEXT NOT NULL,
                    top_frame_site_key TEXT NOT NULL,
                    name TEXT NOT NULL,
                    value TEXT NOT NULL,
                    encrypted_value BLOB NOT NULL,
                    path TEXT NOT NULL,
                    expires_utc INTEGER NOT NULL,
                    is_secure INTEGER NOT NULL,
                    is_httponly INTEGER NOT NULL,
                    last_access_utc INTEGER NOT NULL,
                    has_expires INTEGER NOT NULL,
                    is_persistent INTEGER NOT NULL,
                    priority INTEGER NOT NULL,
                    samesite INTEGER NOT NULL,
                    source_scheme INTEGER NOT NULL,
                    source_port INTEGER NOT NULL,
                    is_same_party INTEGER NOT NULL
                );",
            )
            .expect("creates the table");
        for (host, name, value, plain, expires_utc) in cookies {
            connection
                .execute(
                    "INSERT INTO cookies (
                        creation_utc, host_key, top_frame_site_key, name, value,
                        encrypted_value, path, expires_utc, is_secure, is_httponly,
                        last_access_utc, has_expires, is_persistent, priority, samesite,
                        source_scheme, source_port, is_same_party
                    ) VALUES (0, ?1, '', ?2, ?4, ?3, '/', ?5, 1, 0, 0, 1, 1, 1, 0, 1, 443, 0)",
                    (host, name, value, plain, expires_utc),
                )
                .expect("inserts");
        }
        path
    }

    #[test]
    fn a_v11_cookie_is_read_with_the_keyring_password() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[(
                ".cursor.com",
                "WorkosCursorSessionToken",
                seal(b"a-real-password", b"v11", b"the-session"),
                "",
                0,
            )],
        );

        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);
        let cookies = read(&path, &query, Some("a-real-password")).expect("reads");

        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].value, "the-session");
        assert!(cookies[0].secure);
        assert_eq!(cookies[0].expires_at, None);
    }

    #[test]
    fn a_domain_bound_v11_cookie_is_unwrapped_back_to_its_value() {
        use sha2::Digest;
        let home = TestHome::new();
        // What M130+ writes: the hash of the host before the value, inside the seal.
        let mut plaintext = sha2::Sha256::digest(b".cursor.com").to_vec();
        plaintext.extend_from_slice(b"the-session");
        let path = database(
            &home,
            &[(
                ".cursor.com",
                "WorkosCursorSessionToken",
                seal(b"a-real-password", b"v11", &plaintext),
                "",
                0,
            )],
        );

        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);
        let cookies = read(&path, &query, Some("a-real-password")).expect("reads");

        assert_eq!(cookies[0].value, "the-session");
    }

    #[test]
    fn a_v10_cookie_is_read_without_any_password() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[(
                ".example.com",
                "session",
                seal(BASIC_PASSWORD, b"v10", b"legacy"),
                "",
                0,
            )],
        );

        let query = Query::new(["example.com"], ["session"]);
        let cookies = read(&path, &query, None).expect("reads");

        assert_eq!(cookies[0].value, "legacy");
    }

    #[test]
    fn a_plain_column_value_is_read_without_touching_the_sealed_one() {
        let home = TestHome::new();
        let path = database(&home, &[(".example.com", "consent", Vec::new(), "yes", 0)]);

        let query = Query::new(["example.com"], ["consent"]);
        let cookies = read(&path, &query, None).expect("reads");

        assert_eq!(cookies[0].value, "yes");
    }

    #[test]
    fn a_value_sealed_with_a_password_we_do_not_have_is_skipped_rather_than_faked() {
        let home = TestHome::new();
        let path = database(
            &home,
            &[(
                ".example.com",
                "session",
                seal(b"someone-elses-password", b"v11", b"not-for-us"),
                "",
                0,
            )],
        );

        let query = Query::new(["example.com"], ["session"]);
        // No password at all, and then the wrong password: both must answer nothing,
        // never garbage.
        assert!(read(&path, &query, None).expect("reads").is_empty());
        assert!(
            read(&path, &query, Some("wrong"))
                .expect("reads")
                .is_empty()
        );
    }

    #[test]
    fn a_version_chromium_has_not_invented_yet_is_skipped() {
        let home = TestHome::new();
        let mut unknown = b"v99".to_vec();
        unknown.extend_from_slice(&[0u8; 16]);
        let path = database(&home, &[(".example.com", "session", unknown, "", 0)]);

        let query = Query::new(["example.com"], ["session"]);
        assert!(
            read(&path, &query, Some("anything"))
                .expect("reads")
                .is_empty()
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
                    seal(b"a-real-password", b"v11", b"the-session"),
                    "",
                    0,
                ),
                (
                    ".google.com",
                    "NID",
                    seal(b"a-real-password", b"v11", b"not-ours"),
                    "",
                    0,
                ),
                (
                    ".cursor.com",
                    "_ga",
                    seal(b"a-real-password", b"v11", b"analytics"),
                    "",
                    0,
                ),
            ],
        );

        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);
        let cookies = read(&path, &query, Some("a-real-password")).expect("reads");

        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "WorkosCursorSessionToken");
    }

    #[test]
    fn an_expiry_in_chromium_time_becomes_a_unix_timestamp() {
        let home = TestHome::new();
        // 2026-08-25T00:00:00Z = 1787443200s since 1970 = 13431916800s since 1601.
        let webkit = 13_431_916_800i64 * 1_000_000;
        let path = database(
            &home,
            &[(
                ".example.com",
                "session",
                seal(BASIC_PASSWORD, b"v10", b"legacy"),
                "",
                webkit,
            )],
        );

        let query = Query::new(["example.com"], ["session"]);
        let cookies = read(&path, &query, None).expect("reads");

        assert_eq!(
            cookies[0].expires_at.map(|at| at.as_unix()),
            Some(1_787_443_200)
        );
    }
}
