#![allow(unsafe_code, dead_code)]
//! Manual QA harness compiling the shipped Windows browser modules verbatim.
//! It prints only state/counts, never key material or cookie values.

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
};
use windows::core::PCWSTR;

mod providers {
    pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
}

mod secrets {
    #[derive(Debug, thiserror::Error)]
    pub enum SecretError {
        #[error("locked")]
        Locked,
        #[error("not utf8")]
        NotUtf8,
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub host: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub secure: bool,
    pub expires_at: Option<tidemark_types::Timestamp>,
}

pub struct Query {
    domains: Vec<String>,
    names: Vec<String>,
}

impl Query {
    fn matches(&self, host: &str, name: &str) -> bool {
        let host = host.trim_start_matches('.').to_ascii_lowercase();
        self.domains.iter().any(|domain| {
            let domain = domain.trim_start_matches('.').to_ascii_lowercase();
            host == domain || host.ends_with(&format!(".{domain}"))
        }) && self.names.iter().any(|wanted| wanted == name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    #[error("{path:?}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path:?}: {source}")]
    Database {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("{0}")]
    PlatformUnavailable(&'static str),
}

#[path = "../../../../crates/tidemark-core/src/browser/chromium.rs"]
mod chromium;
#[path = "../../../../crates/tidemark-core/src/browser/safe_storage.rs"]
mod safe_storage;

fn main() {
    let root = std::env::temp_dir().join(format!("tidemark-browser-manual-{}", std::process::id()));
    let profile = root.join("User Data/Default/Network");
    std::fs::create_dir_all(&profile).expect("creates fixture");
    let key = [0x71u8; 32];
    let mut wrapped = b"DPAPI".to_vec();
    wrapped.extend(protect(&key));
    std::fs::write(
        root.join("User Data/Local State"),
        serde_json::json!({"os_crypt":{"encrypted_key":
            base64::engine::general_purpose::STANDARD.encode(wrapped)}})
        .to_string(),
    )
    .expect("writes Local State");
    let database = profile.join("Cookies");
    write_database(&database, &seal(&key, b"manual-round-trip"));
    let loaded = safe_storage::key_for(&database).expect("DPAPI unwraps real fixture");
    let query = Query {
        domains: vec!["example.test".into()],
        names: vec!["session".into()],
    };
    let cookies = chromium::read_windows(&database, &query, &loaded).expect("AES-GCM decrypts");
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].value, "manual-round-trip");
    println!(
        "V10_MANUAL_QA=PASS cookies={} values_printed=0 key_bytes_printed=0",
        cookies.len()
    );

    write_database_replacing(&database, &[b"v10".as_slice(), &[0x55; 28]].concat());
    assert!(
        chromium::read_windows(&database, &query, &loaded)
            .expect("tamper is nonfatal")
            .is_empty()
    );
    println!("TAMPERED_GCM_QA=PASS invented_values=0");
    write_database_replacing(&database, &[b"v20".as_slice(), &[0x55; 28]].concat());
    assert!(matches!(
        chromium::read_windows(&database, &query, &loaded),
        Err(CookieError::PlatformUnavailable(_))
    ));
    println!("V20_STATE_QA=PASS state=windows-unavailable");
    std::fs::write(root.join("User Data/Local State"), b"{malformed").expect("malforms state");
    assert!(safe_storage::key_for(&database).is_err());
    println!("MALFORMED_LOCAL_STATE_QA=PASS invented_key=false");
    std::fs::remove_file(root.join("User Data/Local State")).expect("removes stale state");
    assert!(safe_storage::key_for(&database).is_err());
    println!("STALE_STATE_QA=PASS cached_key_reused=false");
    std::fs::remove_dir_all(&root).expect("cleans fixture");
    println!("V10_MANUAL_CLEANUP=PASS exists={}", root.exists());
}

fn seal(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let nonce = [0x42u8; 12];
    let mut output = b"v10".to_vec();
    output.extend_from_slice(&nonce);
    output.extend(
        Aes256Gcm::new_from_slice(key)
            .expect("key")
            .encrypt(
                &Nonce::try_from(nonce.as_slice()).expect("nonce"),
                plaintext,
            )
            .expect("seal"),
    );
    output
}

fn write_database(path: &Path, encrypted: &[u8]) {
    let connection = rusqlite::Connection::open(path).expect("database");
    connection
        .execute_batch(
            "CREATE TABLE cookies (
        host_key TEXT, name TEXT, encrypted_value BLOB, value TEXT, path TEXT,
        is_secure INTEGER, expires_utc INTEGER); ",
        )
        .expect("schema");
    connection
        .execute(
            "INSERT INTO cookies VALUES
        ('.example.test','session',?1,'','/',1,0)",
            [encrypted],
        )
        .expect("cookie");
}

fn write_database_replacing(path: &Path, encrypted: &[u8]) {
    std::fs::remove_file(path).expect("removes previous database");
    write_database(path, encrypted);
}

fn protect(input: &[u8]) -> Vec<u8> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .expect("DPAPI protect");
    let bytes =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    let _ = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    bytes
}
