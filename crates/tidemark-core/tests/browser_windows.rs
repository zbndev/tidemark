#![cfg(windows)]
#![allow(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use tidemark_core::browser::{BROWSERS, CookieError, Keyring, Query, Store, stores};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
};
use windows::core::PCWSTR;

#[test]
fn synthetic_discovery_and_real_dpapi_v10_round_trip() {
    let fixture = Fixture::new();
    let local = fixture.path.join("Local");
    let roaming = fixture.path.join("Roaming");
    let chrome_profile = local.join("Google/Chrome/User Data/Default");
    let edge_profile = local.join("Microsoft/Edge/User Data/Profile 2");
    let opera_profile = roaming.join("Opera Software/Opera Stable");
    for profile in [&chrome_profile, &edge_profile, &opera_profile] {
        std::fs::create_dir_all(profile.join("Network")).expect("creates profile");
        std::fs::write(profile.join("Network/Cookies"), b"").expect("creates cookie store");
    }

    let old_local = std::env::var_os("LOCALAPPDATA");
    let old_roaming = std::env::var_os("APPDATA");
    // SAFETY: this integration-test binary contains one test, so no other thread reads these
    // process variables while the synthetic machine scan runs.
    unsafe {
        std::env::set_var("LOCALAPPDATA", &local);
        std::env::set_var("APPDATA", &roaming);
    }
    let found: Vec<(&str, String)> = stores()
        .into_iter()
        .map(|store| (store.browser.slug, store.profile))
        .collect();
    restore_env("LOCALAPPDATA", old_local);
    restore_env("APPDATA", old_roaming);
    assert_eq!(
        found,
        [
            ("chrome", "Default".to_owned()),
            ("edge", "Profile 2".to_owned()),
            ("opera", "Opera Stable".to_owned()),
        ]
    );

    let key = [0x6du8; 32];
    let mut wrapped = b"DPAPI".to_vec();
    wrapped.extend(protect(&key));
    std::fs::write(
        local.join("Google/Chrome/User Data/Local State"),
        serde_json::json!({
            "os_crypt": {
                "encrypted_key": base64::engine::general_purpose::STANDARD.encode(wrapped)
            }
        })
        .to_string(),
    )
    .expect("writes Local State");
    let database = chrome_profile.join("Network/Cookies");
    write_cookie_database(&database, &seal(&key, b"round-trip"));
    let chrome = *BROWSERS
        .iter()
        .find(|browser| browser.slug == "chrome")
        .expect("Chrome is registered");
    let store = Store {
        browser: chrome,
        profile: "Default".to_owned(),
        path: database.clone(),
    };
    let query = Query::new(["example.test"], ["session"]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let cookies = runtime
        .block_on(store.cookies(&query, &Keyring))
        .expect("v10 decrypts");
    assert_eq!(cookies.len(), 1);
    assert_eq!(cookies[0].value, "round-trip");

    write_cookie_database(&database, &[b"v10".as_slice(), &[0x55; 28]].concat());
    assert!(
        runtime
            .block_on(store.cookies(&query, &Keyring))
            .expect("tampering is an absent value")
            .is_empty()
    );
    write_cookie_database(&database, &[b"v20".as_slice(), &[0x55; 28]].concat());
    assert!(matches!(
        runtime.block_on(store.cookies(&query, &Keyring)),
        Err(CookieError::PlatformUnavailable(_))
    ));

    std::fs::write(
        local.join("Google/Chrome/User Data/Local State"),
        b"{malformed",
    )
    .expect("malforms Local State");
    assert!(matches!(
        runtime.block_on(store.cookies(&query, &Keyring)),
        Err(CookieError::PlatformUnavailable(_))
    ));
}

fn seal(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let nonce = [0x37u8; 12];
    let mut value = b"v10".to_vec();
    value.extend_from_slice(&nonce);
    value.extend(
        Aes256Gcm::new_from_slice(key)
            .expect("key")
            .encrypt(
                &Nonce::try_from(nonce.as_slice()).expect("nonce"),
                plaintext,
            )
            .expect("seals"),
    );
    value
}

fn write_cookie_database(path: &Path, encrypted: &[u8]) {
    let _ = std::fs::remove_file(path);
    let connection = rusqlite::Connection::open(path).expect("opens cookie database");
    connection
        .execute_batch(
            "CREATE TABLE cookies (
                host_key TEXT NOT NULL, name TEXT NOT NULL, encrypted_value BLOB NOT NULL,
                value TEXT NOT NULL, path TEXT NOT NULL, is_secure INTEGER NOT NULL,
                expires_utc INTEGER NOT NULL
            );",
        )
        .expect("creates cookie table");
    connection
        .execute(
            "INSERT INTO cookies VALUES ('.example.test', 'session', ?1, '', '/', 1, 0)",
            [encrypted],
        )
        .expect("inserts cookie");
}

fn protect(input: &[u8]) -> Vec<u8> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(input.len()).expect("fixture key length"),
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: the input blob points to its initialized slice and output is writable.
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
    .expect("the running user's DPAPI protects the fixture key");
    assert!(!output.pbData.is_null());
    // SAFETY: DPAPI returned `cbData` initialized bytes in a LocalFree allocation.
    let protected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    // SAFETY: this is exactly the allocation returned by CryptProtectData.
    let _ = unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
    protected
}

fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
    // SAFETY: see the single-test process invariant at the mutation site.
    unsafe {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tidemark-windows-browser-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("creates fixture");
        Self { path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
