#![cfg(windows)]

use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};

use fs4::{FileExt, TryLockError};
use serde_json::json;
use tidemark_core::oauth_file::{CredentialFile, CredentialFileError, Field, UpdateOutcome};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tidemark-oauth-windows-test-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("test directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_credentials(path: &Path, access_token: &str) {
    fs::write(
        path,
        serde_json::to_vec(&json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": "fixture-refresh"
            },
            "unrelated": { "ownedByCli": true }
        }))
        .expect("serialize fixture"),
    )
    .expect("write fixture");
}

fn symlink_unavailable(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(1314) // ERROR_PRIVILEGE_NOT_HELD
        || matches!(
            error.kind(),
            ErrorKind::PermissionDenied | ErrorKind::Unsupported
        )
}

/// A directory junction needs no symlink privilege, so the junction attack case
/// never skips: junctions are the reparse point an attacker can create freely.
fn create_junction(target: &Path, link: &Path) {
    let output = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .output()
        .expect("spawn mklink");
    assert!(
        output.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn file_symlink_attack_is_rejected() {
    let dir = TestDir::new();
    let real = dir.join("real.json");
    let linked = dir.join("linked.json");
    write_credentials(&real, "old");
    if let Err(error) = symlink_file(&real, &linked) {
        if symlink_unavailable(&error) {
            eprintln!(
                "SKIP: Windows user lacks CreateSymbolicLink privilege ({error}); junction coverage remains active"
            );
            return;
        }
        panic!("create file symlink: {error}");
    }

    let file = CredentialFile::new(linked.clone(), linked);
    assert!(file.lock().is_err(), "secure open followed a file symlink");
}

#[test]
fn directory_junction_attack_is_rejected() {
    let dir = TestDir::new();
    let path = dir.join("credentials.json");
    let redirected = dir.join("redirected");
    let vendor_lock = dir.join("vendor.lock");
    write_credentials(&path, "old");
    fs::create_dir(&redirected).expect("junction target");
    // A junction needs no symlink privilege, so this attack case never skips.
    create_junction(&redirected, &vendor_lock);

    let locked = CredentialFile::new(path.clone(), path)
        .coordinated_by(vendor_lock)
        .lock()
        .expect("credential locks");
    assert!(matches!(
        locked.update_top_level(
            "claudeAiOauth",
            ("refreshToken", "fixture-refresh"),
            &[(Field::Subtree("accessToken"), json!("must-not-land"))],
        ),
        Err(CredentialFileError::Contended)
    ));
}

#[test]
fn atomic_replace_exposes_only_complete_states() {
    let dir = TestDir::new();
    let path = dir.join("credentials.json");
    let old = "o".repeat(128 * 1024);
    let new = "n".repeat(128 * 1024);
    write_credentials(&path, &old);

    let start = Arc::new(Barrier::new(2));
    let done = Arc::new(AtomicBool::new(false));
    let writer_path = path.clone();
    let writer_start = Arc::clone(&start);
    let writer_done = Arc::clone(&done);
    let writer_value = new.clone();
    let writer = std::thread::spawn(move || {
        let locked = CredentialFile::new(writer_path.clone(), writer_path)
            .lock()
            .expect("writer lock");
        writer_start.wait();
        let outcome = locked
            .update_top_level(
                "claudeAiOauth",
                ("refreshToken", "fixture-refresh"),
                &[(Field::Subtree("accessToken"), json!(writer_value))],
            )
            .expect("atomic publish");
        writer_done.store(true, Ordering::Release);
        outcome
    });

    start.wait();
    let mut observations = 0;
    while !done.load(Ordering::Acquire) {
        // Windows byte-range locks are mandatory: while the writer holds the update
        // lock, a reader may be denied outright (os error 33). Being denied is not a
        // torn state — only a successful read is checked for completeness.
        match fs::read(&path) {
            Ok(bytes) => {
                let document: serde_json::Value =
                    serde_json::from_slice(&bytes).expect("reader sees complete JSON");
                let token = document["claudeAiOauth"]["accessToken"]
                    .as_str()
                    .expect("complete token");
                assert!(
                    token == old || token == new,
                    "reader observed a partial state"
                );
                observations += 1;
            }
            Err(error) if error.raw_os_error() == Some(33) => {}
            Err(error) => panic!("unexpected reader error: {error}"),
        }
        std::thread::yield_now();
    }
    // After the lock release a successful read is guaranteed, pinning the final state
    // deterministically instead of relying on the spin loop having observed a window.
    let final_document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("final read after lock release"))
            .expect("final read is complete JSON");
    assert_eq!(
        final_document["claudeAiOauth"]["accessToken"],
        json!(new),
        "published final state"
    );
    assert_eq!(
        writer.join().expect("writer thread"),
        UpdateOutcome::Published
    );
    assert!(observations > 0, "reader observed the publish window");
    assert!(
        fs::read_dir(&dir.0)
            .expect("directory readable")
            .all(|entry| !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp")),
        "successful publish left a staging file"
    );
}

#[test]
fn update_lock_excludes_a_concurrent_owner() {
    let dir = TestDir::new();
    let path = dir.join("credentials.json");
    write_credentials(&path, "old");

    let _guard = CredentialFile::new(path.clone(), path.clone())
        .lock()
        .expect("first lock");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("credentials.json.tidemark.lock"))
        .expect("lock file");
    assert!(matches!(
        FileExt::try_lock(&contender),
        Err(TryLockError::WouldBlock)
    ));
    let target_contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("target file");
    assert!(matches!(
        FileExt::try_lock(&target_contender),
        Err(TryLockError::WouldBlock)
    ));
}
