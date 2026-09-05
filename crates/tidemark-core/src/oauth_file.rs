//! Safe updates to OAuth credential files owned by third-party CLIs.

use std::fs::{self, File};
use std::io::Read;
use std::ops::Range;
#[cfg(all(test, unix))]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fs4::FileExt;

use platform::VendorWriteLock;

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

    /// Reads a vendor-owned credential without taking the exclusive update lock.
    ///
    /// An otherwise-live CLI can hold that lock while it is idle. Reading does not
    /// mutate the document, so only a token rotation needs the exclusive lock.
    pub fn read_json(&self) -> Result<serde_json::Value, CredentialFileError> {
        let mut file = platform::open_read_file(&self.path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Takes the update lock.
    pub fn lock(&self) -> Result<LockedCredentialFile, CredentialFileError> {
        let lock_path = lock_path(&self.path)?;
        reject_non_regular_if_present(&lock_path)?;
        let lock = platform::open_lock_file(&lock_path, &self.path)?;
        try_lock(&lock)?;
        let target_lock = platform::open_target_file(&self.path)?;
        try_lock(&target_lock)?;
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

/// Where a field lives in a credential document.
///
/// Two of the five providers keep part of the credential state outside the token subtree:
/// Codex's `last_refresh` sits at the document root beside `tokens`. Both addresses go
/// through one guarded publish, because a rotation that landed as two writes could be
/// interrupted between them and leave the file describing itself wrongly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field<'a> {
    /// A field of the document root, beside the token subtree.
    Root(&'a str),
    /// A field inside the guarded token subtree.
    Subtree(&'a str),
}

impl<'a> Field<'a> {
    /// The field's own name, whichever object it lives in.
    pub fn name(self) -> &'a str {
        match self {
            Self::Root(name) | Self::Subtree(name) => name,
        }
    }
}

/// Applies field updates to a document held in memory.
///
/// The counterpart of [`LockedCredentialFile::update_top_level`] for a credential Tidemark
/// owns outright — a login performed from the interface, kept in the Secret Service. None
/// of the file protocol applies to those bytes: no vendor process writes them, so there is
/// nothing to compare-and-swap against and nothing to preserve a backup of. What survives
/// the move is the [`Field`] addressing, so a provider describes a rotation once and both
/// stores understand it.
pub fn apply_fields(
    document: &mut serde_json::Value,
    subtree: &str,
    updates: &[(Field<'_>, serde_json::Value)],
) -> Result<(), CredentialFileError> {
    for (field, value) in updates {
        let object = match field {
            Field::Root(_) => document
                .as_object_mut()
                .ok_or(CredentialFileError::RootNotObject),
            Field::Subtree(_) => document
                .get_mut(subtree)
                .and_then(serde_json::Value::as_object_mut)
                .ok_or(CredentialFileError::SubtreeNotObject),
        }?;
        object.insert(field.name().to_owned(), value.clone());
    }
    Ok(())
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
    /// Reads the current document while the update lock is held.
    ///
    /// Windows byte-range locks are mandatory: a fresh handle on the locked destination
    /// would be denied with `ERROR_LOCK_VIOLATION` even in the locking process itself, so
    /// the locked target handle is the reader. Unix locks are advisory, where the plain
    /// path read stays exactly as it was.
    fn read_current(&self) -> std::io::Result<Vec<u8>> {
        #[cfg(windows)]
        {
            use std::io::Read;
            let mut bytes = Vec::new();
            (&self.target_lock).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        #[cfg(unix)]
        {
            fs::read(&self.path)
        }
    }
    /// Reads the complete JSON document.
    pub fn read_json(&self) -> Result<serde_json::Value, CredentialFileError> {
        let bytes = self.read_current()?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Refuses ambiguous duplicate keys before an irreversible token exchange begins.
    pub fn preflight_unique_fields(
        &self,
        key: &str,
        fields: &[Field<'_>],
    ) -> Result<(), CredentialFileError> {
        let bytes = self.read_current()?;
        let _: serde_json::Value = serde_json::from_slice(&bytes)?;
        for field in fields {
            let object = enclosing_object_span(&bytes, key, *field)?;
            let _ = object_field_value_span(&bytes, object, field.name())?;
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
        updates: &[(Field<'_>, serde_json::Value)],
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
        let original = self.read_current()?;
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
            update_field(&mut updated, key, *field, value)?;
        }
        platform::atomic_private_publish(&self.path, &updated, || {
            #[cfg(windows)]
            {
                // Windows byte-range locks are mandatory: the lock this process holds on
                // the destination would block the replace move with a lock violation, so
                // it is released for the atomic move itself. Unix renames ignore POSIX
                // locks, so the Linux arm keeps its lock held to the very end.
                FileExt::unlock(&self.target_lock)?;
            }
            if let Some(vendor_lock) = _vendor_lock.as_ref() {
                vendor_lock.verify_ownership()?;
            }
            Ok(())
        })?;
        Ok(UpdateOutcome::Published)
    }

    /// Replaces or adds fields at the document root, but only when the document is still
    /// the one the caller's external work was based on.
    ///
    /// The counterpart of [`Self::update_top_level`] for a credential file with a flat
    /// shape — Gemini's `oauth_creds.json` keeps its tokens at the root, with no subtree
    /// and no single source value to compare against, so the caller says in `unchanged`
    /// what "still the same document" means for its credential. The document is reread
    /// here, inside the update, and the comparison reads the same bytes the publish
    /// merges into: a vendor that replaces the file while the caller worked — the Gemini
    /// CLI honors no lock — is never overlaid, and the update is dropped as
    /// [`UpdateOutcome::SourceChanged`]. Every unnamed field keeps its bytes, as
    /// everywhere else in this module.
    pub fn update_root_fields_if_unchanged<U>(
        &self,
        unchanged: U,
        updates: &[(&str, serde_json::Value)],
    ) -> Result<UpdateOutcome, CredentialFileError>
    where
        U: Fn(&serde_json::Value) -> bool,
    {
        if self.path != self.canonical {
            return Err(CredentialFileError::NotCanonical {
                path: self.path.clone(),
                canonical: self.canonical.clone(),
            });
        }
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(CredentialFileError::NotRegularFile(self.path.clone()));
        }
        let original = self.read_current()?;
        let document: serde_json::Value = serde_json::from_slice(&original)?;
        if !unchanged(&document) {
            return Ok(UpdateOutcome::SourceChanged);
        }
        let mut updated = original;
        for (name, value) in updates {
            update_field(&mut updated, "", Field::Root(name), value)?;
        }
        platform::atomic_private_publish(&self.path, &updated, || {
            #[cfg(windows)]
            {
                // See update_top_level: the mandatory destination lock must not cover
                // the replace move on Windows.
                FileExt::unlock(&self.target_lock)?;
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
        let bytes = self.read_current()?;
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CredentialFileError::InvalidPath(self.path.clone()))?;
        let backup = self.path.with_file_name(format!("{name}.tidemark-backup"));
        platform::atomic_private_publish(&backup, &bytes, || Ok(()))?;
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

/// Acquires an exclusive advisory lock without mistaking a real I/O failure for contention.
fn try_lock(file: &File) -> Result<(), CredentialFileError> {
    match FileExt::try_lock(file) {
        Ok(()) => Ok(()),
        Err(fs4::TryLockError::WouldBlock) => Err(CredentialFileError::Contended),
        Err(fs4::TryLockError::Error(error)) => Err(error.into()),
    }
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

/// The span of the object a field lives in: the document root, or the token subtree.
fn enclosing_object_span(
    bytes: &[u8],
    top_level_key: &str,
    field: Field<'_>,
) -> Result<Range<usize>, CredentialFileError> {
    let object = match field {
        Field::Root(_) => {
            let mut cursor = 0;
            skip_whitespace(bytes, &mut cursor);
            if bytes.get(cursor) != Some(&b'{') {
                return Err(CredentialFileError::RootNotObject);
            }
            cursor..skip_value(bytes, cursor)?
        }
        Field::Subtree(_) => {
            let object = top_level_value_span(bytes, top_level_key)?;
            if bytes.get(object.start) != Some(&b'{') {
                return Err(CredentialFileError::SubtreeNotObject);
            }
            object
        }
    };
    Ok(object)
}

fn update_field(
    bytes: &mut Vec<u8>,
    top_level_key: &str,
    field: Field<'_>,
    value: &serde_json::Value,
) -> Result<(), CredentialFileError> {
    let object = enclosing_object_span(bytes, top_level_key, field)?;
    let field = field.name();
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

#[cfg(unix)]
mod platform {
    use std::fs::{self, File, FileTimes, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use super::CredentialFileError;

    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

    pub(super) fn open_lock_file(
        path: &Path,
        credential_path: &Path,
    ) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(CredentialFileError::NotRegularFile(
                credential_path.to_owned(),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }

    pub(super) fn open_target_file(path: &Path) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(CredentialFileError::NotRegularFile(path.to_owned()));
        }
        Ok(file)
    }

    pub(super) fn open_read_file(path: &Path) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(CredentialFileError::NotRegularFile(path.to_owned()));
        }
        Ok(file)
    }

    pub(super) fn atomic_private_publish(
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

    pub(super) struct VendorWriteLock {
        path: PathBuf,
        directory: File,
        device: u64,
        inode: u64,
    }

    impl VendorWriteLock {
        pub(super) fn acquire(path: &Path) -> Result<Self, CredentialFileError> {
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

        pub(super) fn verify_ownership(&self) -> Result<(), CredentialFileError> {
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
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::sync::atomic::{AtomicU64, Ordering};

        use super::*;

        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

        struct TestDir(PathBuf);

        impl TestDir {
            fn new() -> Self {
                let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tidemark-oauth-platform-{}-{serial}",
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

        #[test]
        fn secure_open_is_private_and_never_follows_a_symlink() {
            let dir = TestDir::new();
            let target = dir.join("credentials.json");
            let lock_path = dir.join("credentials.lock");
            fs::write(&target, b"{}").expect("target file");

            let lock = open_lock_file(&lock_path, &target).expect("secure lock open");
            assert_eq!(
                lock.metadata().expect("lock metadata").permissions().mode() & 0o777,
                0o600
            );
            let linked = dir.join("linked.json");
            symlink(&target, &linked).expect("symlink");
            assert!(open_target_file(&linked).is_err());
        }

        #[test]
        fn file_identity_tracks_the_open_directory() {
            let dir = TestDir::new();
            let path = dir.join("vendor.lock");
            let lock = VendorWriteLock::acquire(&path).expect("directory lock");
            let metadata = fs::symlink_metadata(&path).expect("path metadata");

            assert_eq!(lock.device, metadata.dev());
            assert_eq!(lock.inode, metadata.ino());
            fs::remove_dir(&path).expect("remove named directory");
            assert!(!lock.owns_current_path());
        }

        #[test]
        fn vendor_directory_lock_round_trip_and_contention_are_deterministic() {
            let dir = TestDir::new();
            let path = dir.join("vendor.lock");
            let lock = VendorWriteLock::acquire(&path).expect("first lock");

            assert!(matches!(
                VendorWriteLock::acquire(&path),
                Err(CredentialFileError::Contended)
            ));
            lock.verify_ownership().expect("still owns lock");
            drop(lock);
            assert!(!path.exists(), "owned lock directory removed");
        }

        #[test]
        fn private_publish_round_trip_and_failures_leave_no_staging_file() {
            let dir = TestDir::new();
            let path = dir.join("credentials.json");
            fs::write(&path, b"old").expect("old file");

            atomic_private_publish(&path, b"new", || Ok(())).expect("publish");
            assert_eq!(fs::read(&path).expect("published file"), b"new");
            assert_eq!(
                fs::metadata(&path)
                    .expect("published metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );

            let refused =
                atomic_private_publish(&path, b"refused", || Err(CredentialFileError::Contended));
            assert!(matches!(refused, Err(CredentialFileError::Contended)));
            assert_eq!(fs::read(&path).expect("original retained"), b"new");
            assert!(
                fs::read_dir(&dir.0)
                    .expect("directory readable")
                    .all(|entry| !entry
                        .expect("directory entry")
                        .file_name()
                        .to_string_lossy()
                        .contains(".tmp"))
            );
            assert!(atomic_private_publish(&dir.join("missing/file"), b"x", || Ok(())).is_err());
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::fs::{self, File, FileTimes, OpenOptions};
    use std::io::{self, Write};
    use std::mem::size_of;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows::Win32::Security::{
        ACL, ACL_REVISION, AddAccessAllowedAce, DACL_SECURITY_INFORMATION, GetLengthSid,
        GetTokenInformation, InitializeAcl, PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFileInformationByHandle, WRITE_DAC,
    };
    use windows::Win32::System::Threading::OpenProcessToken;

    use super::CredentialFileError;

    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

    fn win_io(error: windows::core::Error) -> io::Error {
        io::Error::other(error)
    }

    fn file_handle(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle())
    }

    /// Read/write plus `WRITE_DAC`: publishing the user-only DACL through
    /// `SetSecurityInfo` needs the handle's write-DAC access right, which generic
    /// read/write does not grant.
    fn write_dac_access() -> u32 {
        (FILE_GENERIC_READ | FILE_GENERIC_WRITE | WRITE_DAC).0
    }

    /// Every handle this module opens shares read/write/delete. The delete share is not
    /// optional: the publish move replaces the destination while this process still holds
    /// the target handle open for its mandatory byte-range lock, and a replace blocked by
    /// the writer's own handle would fail with `ERROR_SHARING_VIOLATION`.
    fn share_all() -> u32 {
        (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0
    }

    fn file_information(file: &File) -> Result<BY_HANDLE_FILE_INFORMATION, CredentialFileError> {
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live kernel handle and `information` is writable for the call.
        unsafe { GetFileInformationByHandle(file_handle(file), &mut information) }
            .map_err(win_io)?;
        Ok(information)
    }

    fn reject_reparse(
        file: &File,
        reported_path: &Path,
    ) -> Result<BY_HANDLE_FILE_INFORMATION, CredentialFileError> {
        let information = file_information(file)?;
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(CredentialFileError::NotRegularFile(
                reported_path.to_owned(),
            ));
        }
        Ok(information)
    }

    fn open_reparse_file(path: &Path, reported_path: &Path) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL).0)
            .access_mode(write_dac_access())
            .share_mode(share_all())
            .open(path)?;
        reject_reparse(&file, reported_path)?;
        if !file.metadata()?.is_file() {
            return Err(CredentialFileError::NotRegularFile(
                reported_path.to_owned(),
            ));
        }
        Ok(file)
    }

    fn open_directory(path: &Path) -> Result<File, CredentialFileError> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(
                (FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL)
                    .0,
            )
            .access_mode(write_dac_access())
            .share_mode(share_all())
            .open(path)?;
        reject_reparse(&directory, path)?;
        if !directory.metadata()?.is_dir() {
            return Err(CredentialFileError::NotRegularFile(path.to_owned()));
        }
        Ok(directory)
    }

    fn set_user_only_acl(file: &File) -> Result<(), CredentialFileError> {
        let mut token = HANDLE::default();
        // `-1` is the documented current-process pseudo-handle and is not closed.
        unsafe { OpenProcessToken(HANDLE(-1_isize as *mut _), TOKEN_QUERY, &mut token) }
            .map_err(win_io)?;
        struct TokenGuard(HANDLE);
        impl Drop for TokenGuard {
            fn drop(&mut self) {
                // SAFETY: OpenProcessToken returned this owned handle.
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
        let _token_guard = TokenGuard(token);

        let mut bytes_needed = 0;
        // The sizing call intentionally fails with ERROR_INSUFFICIENT_BUFFER and sets the size.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes_needed) };
        if bytes_needed == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let words = (bytes_needed as usize).div_ceil(size_of::<usize>());
        let mut token_buffer = vec![0usize; words];
        // SAFETY: the word buffer is suitably aligned and has the size returned above.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(token_buffer.as_mut_ptr().cast()),
                bytes_needed,
                &mut bytes_needed,
            )
        }
        .map_err(win_io)?;
        // SAFETY: TokenUser guarantees a TOKEN_USER at the start of the returned buffer.
        let sid = unsafe { (*(token_buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        let sid_length = unsafe { GetLengthSid(sid) } as usize;
        let acl_bytes = size_of::<ACL>() + size_of::<u32>() * 2 + sid_length;
        let acl_words = acl_bytes.div_ceil(size_of::<usize>());
        let mut acl_buffer = vec![0usize; acl_words];
        let acl = acl_buffer.as_mut_ptr().cast::<ACL>();
        // SAFETY: the aligned backing buffer remains live through SetSecurityInfo; the SID is
        // owned by token_buffer and AddAccessAllowedAce copies it into the ACL.
        unsafe { InitializeAcl(acl, (acl_words * size_of::<usize>()) as u32, ACL_REVISION) }
            .map_err(win_io)?;
        unsafe { AddAccessAllowedAce(acl, ACL_REVISION, FILE_ALL_ACCESS.0, sid) }
            .map_err(win_io)?;
        // A protected DACL with one full-control ACE is Windows' 0600 equivalent: the current
        // user is the sole principal and no directory ACE can broaden access after publication.
        let status = unsafe {
            SetSecurityInfo(
                file_handle(file),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status.0 as i32).into());
        }
        Ok(())
    }

    pub(super) fn open_lock_file(
        path: &Path,
        credential_path: &Path,
    ) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL).0)
            .access_mode(write_dac_access())
            .share_mode(share_all())
            .open(path)?;
        reject_reparse(&file, credential_path)?;
        if !file.metadata()?.is_file() {
            return Err(CredentialFileError::NotRegularFile(
                credential_path.to_owned(),
            ));
        }
        set_user_only_acl(&file)?;
        Ok(file)
    }

    pub(super) fn open_target_file(path: &Path) -> Result<File, CredentialFileError> {
        open_reparse_file(path, path)
    }

    pub(super) fn open_read_file(path: &Path) -> Result<File, CredentialFileError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL).0)
            .access_mode(FILE_GENERIC_READ.0)
            .share_mode(share_all())
            .open(path)?;
        reject_reparse(&file, path)?;
        if !file.metadata()?.is_file() {
            return Err(CredentialFileError::NotRegularFile(path.to_owned()));
        }
        Ok(file)
    }

    pub(super) fn atomic_private_publish(
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
                .read(true)
                .write(true)
                .create_new(true)
                .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_ATTRIBUTE_NORMAL).0)
                .access_mode(write_dac_access())
                .share_mode(share_all())
                .open(&candidate)
            {
                Ok(file) => {
                    stage_path = Some(candidate);
                    stage = Some(file);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let stage_path = stage_path.ok_or(CredentialFileError::StageCollisions)?;
        let mut stage_guard = StageGuard(Some(stage_path.clone()));
        let mut stage = stage.expect("path and file are set together");
        reject_reparse(&stage, path)?;
        set_user_only_acl(&stage)?;
        stage.write_all(bytes)?;
        stage.sync_all()?;
        drop(stage);
        before_rename()?;

        // MoveFileExW, not ReplaceFileW: ReplaceFileW needs to open the destination for
        // writing and trips over this process's own mandatory byte-range locks held under
        // the update lock. The staged file already carries the user-only DACL, so there is
        // no destination ACL worth preserving across the replace.
        // A plain `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` is not usable here: it fails
        // with ACCESS_DENIED whenever ANY handle is open on the destination, and this
        // process itself must hold the target handle (opened with FILE_SHARE_DELETE) up
        // to the move for its mandatory byte-range lock. `std::fs::rename` performs the
        // replace through the POSIX-semantics rename-by-handle path first, which admits
        // concurrent FILE_SHARE_DELETE handles, and falls back to MoveFileExW. Errors 5
        // (ACCESS_DENIED), 32 (SHARING_VIOLATION) and 33 (LOCK_VIOLATION) from a reader
        // that raced the replace are transient: retry a bounded, deterministic number of
        // times instead of failing the publish or spinning forever.
        let mut published = false;
        let mut last_error = None;
        for _ in 0..25 {
            match fs::rename(&stage_path, path) {
                Ok(()) => {
                    published = true;
                    break;
                }
                Err(error) if matches!(error.raw_os_error(), Some(5) | Some(32) | Some(33)) => {
                    last_error = Some(error);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        }
        if !published {
            return Err(CredentialFileError::Io(last_error.unwrap_or_else(|| {
                io::Error::other("publish move did not complete within the retry budget")
            })));
        }
        stage_guard.0 = None;
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

    pub(super) struct VendorWriteLock {
        path: PathBuf,
        directory: File,
        volume_serial: u32,
        file_id: u64,
    }

    impl VendorWriteLock {
        pub(super) fn acquire(path: &Path) -> Result<Self, CredentialFileError> {
            match fs::create_dir(path) {
                Ok(()) => {
                    let directory = match open_directory(path) {
                        Ok(directory) => directory,
                        Err(error) => {
                            let _ = fs::remove_dir(path);
                            return Err(error);
                        }
                    };
                    set_user_only_acl(&directory)?;
                    let identity = file_information(&directory)?;
                    Ok(Self {
                        path: path.to_owned(),
                        directory,
                        volume_serial: identity.dwVolumeSerialNumber,
                        file_id: u64::from(identity.nFileIndexHigh) << 32
                            | u64::from(identity.nFileIndexLow),
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    Err(CredentialFileError::Contended)
                }
                Err(error) => Err(error.into()),
            }
        }

        pub(super) fn verify_ownership(&self) -> Result<(), CredentialFileError> {
            self.directory
                .set_times(FileTimes::new().set_modified(SystemTime::now()))?;
            if self.owns_current_path() {
                Ok(())
            } else {
                Err(CredentialFileError::Contended)
            }
        }

        fn owns_current_path(&self) -> bool {
            open_directory(&self.path)
                .and_then(|directory| file_information(&directory))
                .is_ok_and(|identity| {
                    identity.dwVolumeSerialNumber == self.volume_serial
                        && (u64::from(identity.nFileIndexHigh) << 32
                            | u64::from(identity.nFileIndexLow))
                            == self.file_id
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_file_can_be_read_without_an_update_lock() {
        let directory = std::env::temp_dir().join(format!(
            "tidemark-read-only-credential-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).expect("test directory");
        let path = directory.join("auth.json");
        fs::write(&path, r#"{"tokens":{"access_token":"live"}}"#).expect("credential file");

        let document = CredentialFile::new(path.clone(), path)
            .read_json()
            .expect("read-only credential read");

        assert_eq!(document["tokens"]["access_token"], "live");
        fs::remove_dir_all(directory).expect("test cleanup");
    }

    #[test]
    fn a_root_update_replaces_named_fields_and_keeps_the_rest_of_the_document() {
        let dir = std::env::temp_dir().join(format!("tidemark-root-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("test directory");
        let path = dir.join("oauth_creds.json");
        fs::write(
            &path,
            r#"{"access_token":"old","refresh_token":"keep","unrelated":{"nested":true}}"#,
        )
        .expect("write credentials");

        let locked = CredentialFile::new(path.clone(), path.clone())
            .lock()
            .expect("lock acquired");
        locked
            .update_root_fields_if_unchanged(
                |_| true,
                &[
                    ("access_token", serde_json::Value::from("new")),
                    ("expiry_date", serde_json::Value::from(123_i64)),
                ],
            )
            .expect("update published");
        drop(locked);

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reread")).expect("JSON");
        assert_eq!(updated["access_token"], "new");
        assert_eq!(updated["expiry_date"], 123);
        assert_eq!(updated["refresh_token"], "keep");
        assert_eq!(updated["unrelated"]["nested"], true);
        #[cfg(unix)]
        {
            let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "published private");
        }
        fs::remove_dir_all(&dir).expect("test cleanup");
    }

    // Pins the unix advisory-lock + root-replace discipline; on Windows the
    // mandatory LockFileEx region lock makes the concurrent-handle fixture
    // fail with os error 33. Windows semantics are todo 18's mirror module.
    #[cfg(unix)]
    #[test]
    fn a_root_update_on_a_file_replaced_underneath_is_dropped_rather_than_overlaid() {
        // The Gemini CLI honors no lock and replaces the file atomically: comparing
        // before the publish leaves a check-then-write race, so the gate must read the
        // same bytes the merge does.
        let dir = std::env::temp_dir().join(format!(
            "tidemark-root-cas-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).expect("test directory");
        let path = dir.join("oauth_creds.json");
        fs::write(
            &path,
            r#"{"access_token":"old","refresh_token":"first","unrelated":"kept"}"#,
        )
        .expect("write credentials");

        let locked = CredentialFile::new(path.clone(), path.clone())
            .lock()
            .expect("lock acquired");
        // The vendor's atomic replacement lands while the caller's exchange runs.
        fs::write(
            &path,
            r#"{"access_token":"cli","refresh_token":"second","unrelated":"cli-kept"}"#,
        )
        .expect("the CLI replaces the file");
        let outcome = locked
            .update_root_fields_if_unchanged(
                |document| document.get("refresh_token") == Some(&serde_json::Value::from("first")),
                &[("access_token", serde_json::Value::from("new"))],
            )
            .expect("no filesystem error");
        drop(locked);

        assert_eq!(outcome, UpdateOutcome::SourceChanged);
        let after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).expect("reread")).expect("JSON");
        assert_eq!(after["access_token"], "cli");
        assert_eq!(after["refresh_token"], "second");
        assert_eq!(after["unrelated"], "cli-kept");
        fs::remove_dir_all(&dir).expect("test cleanup");
    }

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
