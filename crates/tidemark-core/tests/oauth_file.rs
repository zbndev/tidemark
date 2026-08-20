use std::fs;
use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use fs4::fs_std::FileExt;
use serde_json::json;
use tidemark_core::oauth_file::{CredentialFile, CredentialFileError, UpdateOutcome};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "tidemark-oauth-test-{}-{serial}",
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

fn copy_real_shape(path: &Path) -> serde_json::Value {
    let bytes = include_bytes!("fixtures/claude-credentials.json");
    fs::write(path, bytes).expect("fixture copied");
    serde_json::from_slice(bytes).expect("fixture is JSON")
}

#[test]
fn replacing_the_token_subtree_preserves_every_unrelated_value() {
    let dir = TestDir::new();
    let path = dir.join(".credentials.json");
    let before = copy_real_shape(&path);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("set broad mode");
    let file = CredentialFile::new(path.clone(), path.clone());
    let replacement = json!({
        "accessToken": "fixture-new-access",
        "refreshToken": "fixture-new-refresh",
        "expiresAt": 1788000000000_i64,
        "refreshTokenExpiresAt": 1790000000000_i64,
        "scopes": ["user:inference"],
        "subscriptionType": "pro",
        "rateLimitTier": "default_claude_ai"
    });

    let locked = file.lock().expect("lock acquired");
    let updates: Vec<(&str, serde_json::Value)> = replacement
        .as_object()
        .expect("replacement object")
        .iter()
        .map(|(key, value)| (key.as_str(), value.clone()))
        .collect();
    let outcome = locked
        .update_top_level(
            "claudeAiOauth",
            ("refreshToken", "fixture-old-refresh"),
            &updates,
        )
        .expect("private atomic publish");
    assert_eq!(outcome, UpdateOutcome::Published);
    drop(locked);

    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("published file")).expect("valid JSON");
    assert_eq!(after["claudeAiOauth"], replacement);
    assert_eq!(after["mcpOAuth"], before["mcpOAuth"]);
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o600
    );
    let leftovers: Vec<_> = fs::read_dir(&dir.0)
        .expect("directory readable")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging files left behind: {leftovers:?}"
    );
}

#[test]
fn a_discovered_noncanonical_copy_is_readable_but_never_writable() {
    let dir = TestDir::new();
    let canonical = dir.join("canonical.json");
    let discovered = dir.join("copy.json");
    let before = copy_real_shape(&discovered);
    let file = CredentialFile::new(discovered.clone(), canonical);

    let locked = file.lock().expect("reading a copy is permitted");
    assert_eq!(locked.read_json().expect("copy parses"), before);
    assert!(matches!(
        locked.update_top_level(
            "claudeAiOauth",
            ("refreshToken", "fixture-old-refresh"),
            &[("accessToken", json!("must-not-land"))]
        ),
        Err(CredentialFileError::NotCanonical { .. })
    ));
    drop(locked);

    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(discovered).expect("copy remains")).expect("valid JSON");
    assert_eq!(after, before);
}

#[test]
fn the_update_guard_holds_an_exclusive_advisory_lock() {
    let dir = TestDir::new();
    let path = dir.join(".credentials.json");
    copy_real_shape(&path);
    let file = CredentialFile::new(path.clone(), path);

    let _guard = file.lock().expect("first lock acquired");
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(".credentials.json.tidemark.lock"))
        .expect("lock file exists");
    assert!(!contender.try_lock_exclusive().expect("contention checked"));
    let target_contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(".credentials.json"))
        .expect("credential file exists");
    assert!(
        !target_contender
            .try_lock_exclusive()
            .expect("target contention checked")
    );
}

#[test]
fn a_concurrent_token_rotation_is_never_overwritten() {
    let dir = TestDir::new();
    let path = dir.join(".credentials.json");
    copy_real_shape(&path);
    let file = CredentialFile::new(path.clone(), path.clone());
    let locked = file.lock().expect("lock acquired");

    let mut concurrent = locked.read_json().expect("fixture parses");
    concurrent["claudeAiOauth"]["accessToken"] = json!("cli-new-access");
    concurrent["claudeAiOauth"]["refreshToken"] = json!("cli-new-refresh");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&concurrent).expect("serialize concurrent write"),
    )
    .expect("concurrent publish");

    let outcome = locked
        .update_top_level(
            "claudeAiOauth",
            ("refreshToken", "fixture-old-refresh"),
            &[
                ("accessToken", json!("tidemark-new-access")),
                ("refreshToken", json!("tidemark-new-refresh")),
            ],
        )
        .expect("race is handled, not an I/O failure");
    assert_eq!(outcome, UpdateOutcome::SourceChanged);
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("current file")).expect("current JSON");
    assert_eq!(after["claudeAiOauth"]["accessToken"], "cli-new-access");
    assert_eq!(after["claudeAiOauth"]["refreshToken"], "cli-new-refresh");
}

#[test]
fn bytes_outside_the_replaced_subtree_are_unchanged() {
    let dir = TestDir::new();
    let path = dir.join("compact.json");
    let original = b"{\"claudeAiOauth\" : {\"accessToken\":\"old\",\"refreshToken\":\"old-refresh\"}  ,\n\t\"mcpOAuth\" : { \"odd\" : [1, 2, 3] }\n}\n";
    fs::write(&path, original).expect("compact fixture");
    let file = CredentialFile::new(path.clone(), path.clone());
    let locked = file.lock().expect("lock acquired");

    assert_eq!(
        locked
            .update_top_level(
                "claudeAiOauth",
                ("refreshToken", "old-refresh"),
                &[("accessToken", json!("new"))],
            )
            .expect("publish"),
        UpdateOutcome::Published
    );

    let after = fs::read(path).expect("updated bytes");
    let expected = b"{\"claudeAiOauth\" : {\"accessToken\":\"new\",\"refreshToken\":\"old-refresh\"}  ,\n\t\"mcpOAuth\" : { \"odd\" : [1, 2, 3] }\n}\n";
    assert_eq!(after, expected, "unrelated formatting changed");
}

#[test]
fn a_missing_expiry_field_is_appended_without_reformatting_existing_fields() {
    let dir = TestDir::new();
    let path = dir.join("missing-expiry.json");
    let original =
        b"{\"claudeAiOauth\": { \"accessToken\" : \"old\", \"refreshToken\":\"refresh\" }}\n";
    fs::write(&path, original).expect("fixture copied");
    let file = CredentialFile::new(path.clone(), path.clone());
    let locked = file.lock().expect("lock acquired");

    assert_eq!(
        locked
            .update_top_level(
                "claudeAiOauth",
                ("refreshToken", "refresh"),
                &[("expiresAt", json!(123_i64))],
            )
            .expect("publish"),
        UpdateOutcome::Published
    );

    assert_eq!(
        fs::read(path).expect("updated bytes"),
        b"{\"claudeAiOauth\": { \"accessToken\" : \"old\", \"refreshToken\":\"refresh\" ,\"expiresAt\":123}}\n"
    );
}

#[test]
fn backup_is_an_exact_private_copy_created_before_exchange() {
    let dir = TestDir::new();
    let path = dir.join(".credentials.json");
    let original = include_bytes!("fixtures/claude-credentials.json");
    fs::write(&path, original).expect("fixture copied");
    let file = CredentialFile::new(path.clone(), path);
    let locked = file.lock().expect("lock acquired");

    let backup = locked.backup().expect("backup published");

    assert_eq!(fs::read(&backup).expect("backup readable"), original);
    assert_eq!(
        fs::metadata(backup)
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn the_vendor_write_lock_serializes_the_cas_and_publish_window() {
    let dir = TestDir::new();
    let path = dir.join(".credentials.json");
    copy_real_shape(&path);
    let vendor_lock = dir.join(".storage-write.lock");
    let file = CredentialFile::new(path.clone(), path.clone()).coordinated_by(vendor_lock.clone());
    let locked = file.lock().expect("Tidemark lock acquired");
    fs::create_dir(&vendor_lock).expect("Claude Code owns its write lock");

    assert!(matches!(
        locked.update_top_level(
            "claudeAiOauth",
            ("refreshToken", "fixture-old-refresh"),
            &[("accessToken", json!("must-not-land"))],
        ),
        Err(CredentialFileError::Contended)
    ));
    fs::remove_dir(&vendor_lock).expect("Claude Code releases its write lock");
    assert_eq!(
        locked
            .update_top_level(
                "claudeAiOauth",
                ("refreshToken", "fixture-old-refresh"),
                &[("accessToken", json!("published"))],
            )
            .expect("publish after lock release"),
        UpdateOutcome::Published
    );
    assert!(!vendor_lock.exists(), "our shared write lock was released");
}

#[test]
fn duplicate_oauth_keys_are_rejected_before_exchange() {
    let dir = TestDir::new();
    let path = dir.join("duplicates.json");
    fs::write(
        &path,
        br#"{"claudeAiOauth":{"accessToken":"first","refreshToken":"first"},"claudeAiOauth":{"accessToken":"last","refreshToken":"last"}}"#,
    )
    .expect("duplicate fixture");
    let file = CredentialFile::new(path.clone(), path);
    let locked = file.lock().expect("lock acquired");

    assert!(matches!(
        locked.preflight_unique_fields("claudeAiOauth", &["accessToken", "refreshToken"]),
        Err(CredentialFileError::DuplicateField(_))
    ));
}

#[test]
fn a_symlink_target_is_never_followed() {
    let dir = TestDir::new();
    let real = dir.join("real.json");
    let linked = dir.join("linked.json");
    copy_real_shape(&real);
    symlink(&real, &linked).expect("symlink created");
    let file = CredentialFile::new(linked.clone(), linked);

    assert!(file.lock().is_err(), "O_NOFOLLOW must reject the target");
}
