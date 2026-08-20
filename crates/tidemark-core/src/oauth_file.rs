//! Safe updates to OAuth credential files owned by third-party CLIs.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use fs4::fs_std::FileExt;

static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

/// A third-party credential file and the one path Tidemark is allowed to update.
#[derive(Debug)]
pub struct CredentialFile {
    path: PathBuf,
    canonical: PathBuf,
    write_lock: Option<PathBuf>,
}

impl CredentialFile {
    /// Describes a discovered file and the canonical path that owns refresh persistence.
    pub fn new(path: PathBuf, canonical: PathBuf) -> Self {
        Self {
            path,
            canonical,
            write_lock: None,
        }
    }

    /// Coordinates publication with the vendor's own atomic write lock directory.
    pub fn coordinated_by(mut self, write_lock: PathBuf) -> Self {
        self.write_lock = Some(write_lock);
        self
    }

    /// Takes the update lock.
    pub fn lock(&self) -> Result<LockedCredentialFile, CredentialFileError> {
        let lock_path = lock_path(&self.path)?;
        reject_non_regular_if_present(&lock_path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(lock_path)?;
        if !lock.metadata()?.file_type().is_file() {
            return Err(CredentialFileError::NotRegularFile(self.path.clone()));
        }
        lock.set_permissions(fs::Permissions::from_mode(0o600))?;
        if !lock.try_lock_exclusive()? {
            return Err(CredentialFileError::Contended);
        }
        let target_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        if !target_lock.metadata()?.file_type().is_file() {
            return Err(CredentialFileError::NotRegularFile(self.path.clone()));
        }
        if !target_lock.try_lock_exclusive()? {
            return Err(CredentialFileError::Contended);
        }
        Ok(LockedCredentialFile {
            path: self.path.clone(),
            canonical: self.canonical.clone(),
            write_lock: self.write_lock.clone(),
            lock,
            target_lock,
        })
    }
}

/// A credential file held under its advisory update lock.
#[derive(Debug)]
pub struct LockedCredentialFile {
    path: PathBuf,
    canonical: PathBuf,
    write_lock: Option<PathBuf>,
    lock: File,
    target_lock: File,
}

/// Whether a guarded update was published or a concurrent token owner won the race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The expected source token still owned the file and the update was published.
    Published,
    /// The source token changed while the caller was doing external work; nothing written.
    SourceChanged,
}

impl LockedCredentialFile {
    /// Reads the complete JSON document.
    pub fn read_json(&self) -> Result<serde_json::Value, CredentialFileError> {
        let bytes = std::fs::read(&self.path)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Refuses ambiguous duplicate keys before an irreversible token exchange begins.
    pub fn preflight_unique_fields(
        &self,
        key: &str,
        fields: &[&str],
    ) -> Result<(), CredentialFileError> {
        let bytes = fs::read(&self.path)?;
        let _: serde_json::Value = serde_json::from_slice(&bytes)?;
        let object = top_level_value_span(&bytes, key)?;
        if bytes.get(object.start) != Some(&b'{') {
            return Err(CredentialFileError::SubtreeNotObject);
        }
        for field in fields {
            let _ = object_field_value_span(&bytes, object.clone(), field)?;
        }
        Ok(())
    }

    /// Updates fields inside one top-level subtree when its source token is unchanged.
    ///
    /// The complete document is reread after the caller's external work. Only the values
    /// of named fields are replaced, so whitespace, ordering, and every unrelated byte
    /// stay owned by the CLI.
    pub fn update_top_level(
        &self,
        key: &str,
        expected: (&str, &str),
        updates: &[(&str, serde_json::Value)],
    ) -> Result<UpdateOutcome, CredentialFileError> {
        if self.path != self.canonical {
            return Err(CredentialFileError::NotCanonical {
                path: self.path.clone(),
                canonical: self.canonical.clone(),
            });
        }
        // Claude Code 2.1.237 serializes its own credential mutations with the
        // proper-lockfile directory `.storage-write.lock`. Acquiring that same lock for
        // the CAS and rename closes the otherwise unavoidable check/publish race.
        let _vendor_lock = self
            .write_lock
            .as_deref()
            .map(VendorWriteLock::acquire)
            .transpose()?;
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CredentialFileError::NotRegularFile(self.path.clone()));
        }

        // Re-read under the lock immediately before merging. The caller may have spent
        // time exchanging a refresh token since its first read, and unrelated CLI-owned
        // state written in that interval must survive.
        let original = fs::read(&self.path)?;
        let document: serde_json::Value = serde_json::from_slice(&original)?;
        let root = document
            .as_object()
            .ok_or(CredentialFileError::RootNotObject)?;
        let subtree = root
            .get(key)
            .ok_or_else(|| CredentialFileError::MissingSubtree(key.to_owned()))?
            .as_object()
            .ok_or(CredentialFileError::SubtreeNotObject)?;
        if subtree.get(expected.0).and_then(serde_json::Value::as_str) != Some(expected.1) {
            return Ok(UpdateOutcome::SourceChanged);
        }

        let mut updated = original;
        for (field, value) in updates {
            update_object_field(&mut updated, key, field, value)?;
        }
        atomic_private_publish(&self.path, &updated, || {
            if let Some(vendor_lock) = _vendor_lock.as_ref() {
                vendor_lock.verify_ownership()?;
            }
            Ok(())
        })?;
        Ok(UpdateOutcome::Published)
    }

    /// Writes an exact private backup beside the credential file before token exchange.
    pub fn backup(&self) -> Result<PathBuf, CredentialFileError> {
        if self.path != self.canonical {
            return Err(CredentialFileError::NotCanonical {
                path: self.path.clone(),
                canonical: self.canonical.clone(),
            });
        }
        let bytes = fs::read(&self.path)?;
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CredentialFileError::InvalidPath(self.path.clone()))?;
        let backup = self.path.with_file_name(format!("{name}.tidemark-backup"));
        atomic_private_publish(&backup, &bytes, || Ok(()))?;
        Ok(backup)
    }
}

impl Drop for LockedCredentialFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.target_lock);
        let _ = FileExt::unlock(&self.lock);
    }
}

/// A safe credential-file update could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum CredentialFileError {
    /// Persistence was requested for a file not owned by the canonical CLI path.
    #[error("refusing to update noncanonical credential file {path:?}; expected {canonical:?}")]
    NotCanonical { path: PathBuf, canonical: PathBuf },
    /// Filesystem operation failed.
    #[error("credential file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// The file was not valid JSON.
    #[error("credential file is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The source was not a regular file; publishing over a symlink is never allowed.
    #[error("credential path is not a regular file: {0:?}")]
    NotRegularFile(PathBuf),
    /// The JSON root was not an object.
    #[error("credential file root is not an object")]
    RootNotObject,
    /// The selected token subtree was not an object.
    #[error("credential token subtree is not an object")]
    SubtreeNotObject,
    /// The requested token subtree was absent, so this is not the expected file shape.
    #[error("credential file has no {0:?} subtree")]
    MissingSubtree(String),
    /// The path has no usable parent or file name.
    #[error("credential path is not usable: {0:?}")]
    InvalidPath(PathBuf),
    /// Another credential updater currently owns the advisory lock.
    #[error("credential file is being updated by another process")]
    Contended,
    /// Valid JSON had an unexpected lexical structure while locating the top-level value.
    #[error("credential JSON structure could not be located safely")]
    JsonStructure,
    /// Repeated stale staging files exhausted the bounded O_EXCL retries.
    #[error("could not allocate a unique credential staging file")]
    StageCollisions,
    /// Duplicate credential keys make the effective token ambiguous.
    #[error("credential JSON contains duplicate {0:?} fields")]
    DuplicateField(String),
}

fn reject_non_regular_if_present(path: &Path) -> Result<(), CredentialFileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            Err(CredentialFileError::NotRegularFile(path.to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn lock_path(path: &Path) -> Result<PathBuf, CredentialFileError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CredentialFileError::InvalidPath(path.to_owned()))?;
    Ok(path.with_file_name(format!("{name}.tidemark.lock")))
}

fn top_level_value_span(bytes: &[u8], wanted: &str) -> Result<Range<usize>, CredentialFileError> {
    let mut cursor = 0;
    let mut found = None;
    skip_whitespace(bytes, &mut cursor);
    expect_byte(bytes, &mut cursor, b'{')?;
    loop {
        skip_whitespace(bytes, &mut cursor);
        if bytes.get(cursor) == Some(&b'}') {
            return found.ok_or_else(|| CredentialFileError::MissingSubtree(wanted.to_owned()));
        }
        let key_start = cursor;
        let key_end = skip_string(bytes, cursor)?;
        let key: String = serde_json::from_slice(&bytes[key_start..key_end])?;
        cursor = key_end;
        skip_whitespace(bytes, &mut cursor);
        expect_byte(bytes, &mut cursor, b':')?;
        skip_whitespace(bytes, &mut cursor);
        let value_start = cursor;
        cursor = skip_value(bytes, cursor)?;
        if key == wanted {
            if found.is_some() {
                return Err(CredentialFileError::DuplicateField(wanted.to_owned()));
            }
            found = Some(value_start..cursor);
        }
        skip_whitespace(bytes, &mut cursor);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => {
                return found.ok_or_else(|| CredentialFileError::MissingSubtree(wanted.to_owned()));
            }
            _ => return Err(CredentialFileError::JsonStructure),
        }
    }
}

fn update_object_field(
    bytes: &mut Vec<u8>,
    top_level_key: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), CredentialFileError> {
    let object = top_level_value_span(bytes, top_level_key)?;
    if bytes.get(object.start) != Some(&b'{') {
        return Err(CredentialFileError::SubtreeNotObject);
    }
    let replacement = serde_json::to_vec(value)?;
    if let Some(span) = object_field_value_span(bytes, object.clone(), field)? {
        bytes.splice(span, replacement);
        return Ok(());
    }

    let mut cursor = object.start + 1;
    skip_whitespace(bytes, &mut cursor);
    let separator = if bytes.get(cursor) == Some(&b'}') {
        Vec::new()
    } else {
        vec![b',']
    };
    let mut insertion = separator;
    insertion.extend_from_slice(&serde_json::to_vec(field)?);
    insertion.push(b':');
    insertion.extend_from_slice(&replacement);
    bytes.splice(object.end - 1..object.end - 1, insertion);
    Ok(())
}

fn object_field_value_span(
    bytes: &[u8],
    object: Range<usize>,
    wanted: &str,
) -> Result<Option<Range<usize>>, CredentialFileError> {
    let mut cursor = object.start;
    expect_byte(bytes, &mut cursor, b'{')?;
    let mut found = None;
    loop {
        skip_whitespace(bytes, &mut cursor);
        if bytes.get(cursor) == Some(&b'}') {
            return Ok(found);
        }
        let key_start = cursor;
        let key_end = skip_string(bytes, cursor)?;
        let key: String = serde_json::from_slice(&bytes[key_start..key_end])?;
        cursor = key_end;
        skip_whitespace(bytes, &mut cursor);
        expect_byte(bytes, &mut cursor, b':')?;
        skip_whitespace(bytes, &mut cursor);
        let value_start = cursor;
        cursor = skip_value(bytes, cursor)?;
        if key == wanted {
            if found.is_some() {
                return Err(CredentialFileError::DuplicateField(wanted.to_owned()));
            }
            found = Some(value_start..cursor);
        }
        skip_whitespace(bytes, &mut cursor);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => return Ok(found),
            _ => return Err(CredentialFileError::JsonStructure),
        }
    }
}

fn skip_whitespace(bytes: &[u8], cursor: &mut usize) {
    while bytes
        .get(*cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        *cursor += 1;
    }
}

fn expect_byte(bytes: &[u8], cursor: &mut usize, expected: u8) -> Result<(), CredentialFileError> {
    if bytes.get(*cursor) != Some(&expected) {
        return Err(CredentialFileError::JsonStructure);
    }
    *cursor += 1;
    Ok(())
}

fn skip_string(bytes: &[u8], start: usize) -> Result<usize, CredentialFileError> {
    if bytes.get(start) != Some(&b'"') {
        return Err(CredentialFileError::JsonStructure);
    }
    let mut cursor = start + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor = cursor.saturating_add(2),
            b'"' => return Ok(cursor + 1),
            _ => cursor += 1,
        }
    }
    Err(CredentialFileError::JsonStructure)
}

fn skip_value(bytes: &[u8], start: usize) -> Result<usize, CredentialFileError> {
    match bytes.get(start) {
        Some(b'"') => skip_string(bytes, start),
        Some(open @ (b'{' | b'[')) => {
            let mut stack = vec![*open];
            let mut cursor = start + 1;
            while let Some(byte) = bytes.get(cursor) {
                match byte {
                    b'"' => cursor = skip_string(bytes, cursor)?,
                    b'{' | b'[' => {
                        stack.push(*byte);
                        cursor += 1;
                    }
                    b'}' | b']' => {
                        let expected = if *byte == b'}' { b'{' } else { b'[' };
                        if stack.pop() != Some(expected) {
                            return Err(CredentialFileError::JsonStructure);
                        }
                        cursor += 1;
                        if stack.is_empty() {
                            return Ok(cursor);
                        }
                    }
                    _ => cursor += 1,
                }
            }
            Err(CredentialFileError::JsonStructure)
        }
        Some(_) => {
            let mut cursor = start;
            while bytes.get(cursor).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
            }) {
                cursor += 1;
            }
            (cursor > start)
                .then_some(cursor)
                .ok_or(CredentialFileError::JsonStructure)
        }
        None => Err(CredentialFileError::JsonStructure),
    }
}

fn atomic_private_publish(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> Result<(), CredentialFileError>,
) -> Result<(), CredentialFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| CredentialFileError::InvalidPath(path.to_owned()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CredentialFileError::InvalidPath(path.to_owned()))?;
    let mut stage_path = None;
    let mut stage = None;
    for _ in 0..16 {
        let serial = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.tidemark-{}-{serial}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => {
                stage_path = Some(candidate);
                stage = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let stage_path = stage_path.ok_or(CredentialFileError::StageCollisions)?;
    let mut stage = stage.expect("path and file are set together");
    let mut stage_guard = StageGuard(Some(stage_path.clone()));
    stage.set_permissions(fs::Permissions::from_mode(0o600))?;
    stage.write_all(bytes)?;
    stage.sync_all()?;
    drop(stage);
    before_rename()?;
    fs::rename(&stage_path, path)?;
    stage_guard.0 = None;

    // Persist the directory entry as well as the file contents. The ADR only requires
    // fsync of the staged file, but syncing the parent closes the last crash window in the
    // rename itself on filesystems that journal metadata lazily.
    File::open(parent)?.sync_all()?;
    Ok(())
}

struct StageGuard(Option<PathBuf>);

impl Drop for StageGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

struct VendorWriteLock {
    path: PathBuf,
    directory: File,
    device: u64,
    inode: u64,
}

impl VendorWriteLock {
    fn acquire(path: &Path) -> Result<Self, CredentialFileError> {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => {
                let directory = match OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(path)
                {
                    Ok(directory) => directory,
                    Err(error) => {
                        let _ = fs::remove_dir(path);
                        return Err(error.into());
                    }
                };
                let metadata = directory.metadata()?;
                Ok(Self {
                    path: path.to_owned(),
                    directory,
                    device: metadata.dev(),
                    inode: metadata.ino(),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CredentialFileError::Contended)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn verify_ownership(&self) -> Result<(), CredentialFileError> {
        self.directory
            .set_times(FileTimes::new().set_modified(SystemTime::now()))?;
        if self.owns_current_path() {
            Ok(())
        } else {
            Err(CredentialFileError::Contended)
        }
    }

    fn owns_current_path(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }
}

impl Drop for VendorWriteLock {
    fn drop(&mut self) {
        if self.owns_current_path() {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_vendor_lock_is_neither_used_nor_released() {
        let path =
            std::env::temp_dir().join(format!("tidemark-vendor-lock-test-{}", std::process::id()));
        let _ = fs::remove_dir(&path);
        let lock = VendorWriteLock::acquire(&path).expect("lock acquired");
        fs::remove_dir(&path).expect("stale lock removed by vendor");
        fs::create_dir(&path).expect("vendor acquired a replacement lock");

        assert!(matches!(
            lock.verify_ownership(),
            Err(CredentialFileError::Contended)
        ));
        drop(lock);
        assert!(path.is_dir(), "replacement vendor lock was removed");
        fs::remove_dir(path).expect("test cleanup");
    }
}
