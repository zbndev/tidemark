//! Installer-owned program file transactions. User data is outside this module.

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Component, Path, PathBuf},
};

const MANIFEST: &str = "install-manifest.json";
const JOURNAL: &str = "journal.json";
const MANIFEST_PENDING: &str = ".install-manifest.pending";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid installation path: {0}")]
    InvalidPath(String),
    #[error("invalid installation state: {0}")]
    InvalidState(String),
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{failure}; cleaning the incomplete backup also failed: {cleanup}")]
    Cleanup {
        failure: Box<Error>,
        cleanup: Box<Error>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    install: PathBuf,
    old: Vec<String>,
    incoming: Vec<String>,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    existed: bool,
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> Error {
    Error::Io {
        operation,
        path: path.to_owned(),
        source,
    }
}

/// Validate Windows path semantics even when the transaction tests run on Unix.
fn relative(value: &str) -> Result<String, Error> {
    let normalized = value.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') || normalized.contains(':') {
        return Err(Error::InvalidPath(value.to_owned()));
    }
    for part in normalized.split('/') {
        let device = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved_device = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || device
                .strip_prefix("COM")
                .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
            || device
                .strip_prefix("LPT")
                .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"));
        if part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with([' ', '.'])
            || part
                .chars()
                .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
            || reserved_device
        {
            return Err(Error::InvalidPath(value.to_owned()));
        }
    }
    Ok(normalized)
}

fn list(values: &[String], allow_manifest: bool) -> Result<Vec<String>, Error> {
    let mut keys = BTreeSet::new();
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        let path = relative(value)?;
        let key = path.to_lowercase();
        if (!allow_manifest && key == MANIFEST) || key == MANIFEST_PENDING || !keys.insert(key) {
            return Err(Error::InvalidPath(value.to_owned()));
        }
        result.push(path);
    }
    // A file cannot also be a parent directory, including through case aliases.
    for key in &keys {
        for (index, _) in key.match_indices('/') {
            if keys.contains(&key[..index]) {
                return Err(Error::InvalidPath(key.to_owned()));
            }
        }
    }
    Ok(result)
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_link(&meta) {
                return Err(Error::InvalidPath(path.display().to_string()));
            }
            Ok(Some(meta))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error("inspect", path, e)),
    }
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions, mount points and other reparse points, not only symlinks.
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn inspect(path: &Path) -> Result<Option<fs::Metadata>, Error> {
    let mut current = PathBuf::new();
    let mut leaf = None;
    let components: Vec<_> = path.components().collect();
    for (index, part) in components.iter().enumerate() {
        if matches!(part, Component::ParentDir | Component::CurDir) {
            return Err(Error::InvalidPath(path.display().to_string()));
        }
        current.push(part.as_os_str());
        // A Windows prefix (especially '\\?\C:') is not a filesystem path
        // until the following root separator has been appended.
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        leaf = metadata(&current)?;
        if index + 1 < components.len() && leaf.as_ref().is_some_and(|m| !m.is_dir()) {
            return Err(Error::InvalidPath(current.display().to_string()));
        }
    }
    Ok(leaf)
}

fn absolute(path: &Path) -> Result<PathBuf, Error> {
    let result = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| io_error("get current directory", path, e))?
            .join(path)
    };
    inspect(&result)?;
    // Resolve the existing ancestor for canonical root binding on first install.
    let mut ancestor = result.as_path();
    let mut tail = Vec::new();
    while metadata(ancestor)?.is_none() {
        tail.push(
            ancestor
                .file_name()
                .ok_or_else(|| Error::InvalidPath(result.display().to_string()))?
                .to_owned(),
        );
        ancestor = ancestor
            .parent()
            .ok_or_else(|| Error::InvalidPath(result.display().to_string()))?;
    }
    let mut canonical =
        fs::canonicalize(ancestor).map_err(|e| io_error("canonicalize", ancestor, e))?;
    for part in tail.into_iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}

fn path_key(path: &Path) -> String {
    let key = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

fn roots(install: &Path, state: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let install = absolute(install)?;
    let state = absolute(state)?;
    let i = path_key(&install).trim_end_matches('/').to_owned();
    let s = path_key(&state).trim_end_matches('/').to_owned();
    if i == s || i.starts_with(&format!("{s}/")) || s.starts_with(&format!("{i}/")) {
        return Err(Error::InvalidState(
            "install and backup directories must be disjoint".into(),
        ));
    }
    Ok((install, state))
}

/// Validate a helper state file with the same root and reparse-point boundary.
pub fn state_file(install: &Path, state: &Path, name: &str) -> Result<PathBuf, Error> {
    let (_, state) = roots(install, state)?;
    let path = state.join(relative(name)?);
    regular(&path)?;
    Ok(path)
}

/// Canonical installation identity, including when the directory no longer exists.
pub fn install_root(install: &Path) -> Result<PathBuf, Error> {
    absolute(install)
}

/// Check a saved ownership list after uninstall without trusting its path strings.
pub fn remaining(install: &Path, paths: &[String]) -> Result<Vec<String>, Error> {
    let root = absolute(install)?;
    let mut found = Vec::new();
    for path in list(paths, false)? {
        if regular(&root.join(&path))? {
            found.push(path);
        }
    }
    Ok(found)
}

fn ensure_directory(path: &Path) -> Result<(), Error> {
    if inspect(path)?.is_some_and(|m| !m.is_dir()) {
        return Err(Error::InvalidPath(path.display().to_string()));
    }
    fs::create_dir_all(path).map_err(|e| io_error("create directory", path, e))
}

fn regular(path: &Path) -> Result<bool, Error> {
    match inspect(path)? {
        Some(meta) if !meta.is_file() => Err(Error::InvalidPath(path.display().to_string())),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Error> {
    if !regular(path)? {
        return Err(Error::InvalidState(format!("missing {}", path.display())));
    }
    let bytes = fs::read(path).map_err(|e| io_error("read", path, e))?;
    serde_json::from_slice(&bytes).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })
}

fn write_json<T: Serialize>(file: &mut fs::File, path: &Path, value: &T) -> Result<(), Error> {
    use std::io::Write;
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io_error("write", path, e))
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    inspect(path)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| io_error("create", path, e))?;
    let result = write_json(&mut file, path, value);
    drop(file);
    if let Err(failure) = result {
        return match remove_file(path) {
            Ok(()) => Err(failure),
            Err(cleanup) => Err(Error::Cleanup {
                failure: Box::new(failure),
                cleanup: Box::new(cleanup),
            }),
        };
    }
    Ok(())
}

fn union(old: &[String], incoming: &[String]) -> Result<Vec<String>, Error> {
    let mut result: Vec<String> = Vec::new();
    for path in old.iter().chain(incoming) {
        if let Some(existing) = result
            .iter()
            .find(|p| p.to_lowercase() == path.to_lowercase())
        {
            if existing != path {
                return Err(Error::InvalidPath(path.clone()));
            }
        } else {
            result.push(path.clone());
        }
    }
    result.push(MANIFEST.to_owned());
    list(&result, true)
}

/// Snapshot every old or incoming program file before setup starts writing files.
/// The journal is published last; an incomplete backup cannot be used as a transaction.
pub fn backup(install: &Path, state: &Path, incoming: &[String]) -> Result<(), Error> {
    let incoming = list(incoming, false)?;
    let (install, state) = roots(install, state)?;
    if inspect(&install.join(MANIFEST_PENDING))?.is_some() {
        return Err(Error::InvalidState(
            "pending file exists before backup".into(),
        ));
    }
    let old = owned(&install)?;
    let mut entries = Vec::new();
    for path in union(&old, &incoming)? {
        let existed = regular(&install.join(&path))?;
        entries.push(Entry { path, existed });
    }
    if inspect(&state.join(JOURNAL))?.is_some() || inspect(&state.join("files"))?.is_some() {
        return Err(Error::InvalidState(
            "previous backup exists; restore or finalize it first".into(),
        ));
    }
    ensure_directory(&install)?;
    ensure_directory(&state)?;
    let files = state.join("files");
    fs::create_dir(&files).map_err(|e| io_error("create backup directory", &files, e))?;
    let mut created = Vec::new();
    let result = (|| {
        for (index, entry) in entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.existed)
        {
            let source = install.join(&entry.path);
            regular(&source)?;
            let dest = files.join(index.to_string());
            // The freshly-created files directory was absent before this call.
            // A failed copy can leave a partial destination, so track it first.
            created.push(dest.clone());
            fs::copy(&source, &dest).map_err(|e| io_error("back up", &source, e))?;
            fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .and_then(|f| f.sync_all())
                .map_err(|e| io_error("sync backup", &dest, e))?;
        }
        let path = state.join(JOURNAL);
        inspect(&path)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| io_error("create journal", &path, e))?;
        // Only delete a journal actually created by this invocation.
        created.push(path.clone());
        write_json(
            &mut file,
            &path,
            &Journal {
                version: 1,
                install,
                old,
                incoming,
                entries,
            },
        )
    })();
    if let Err(failure) = result {
        let cleanup = (|| {
            for path in created.iter().rev() {
                remove_file(path)?;
            }
            inspect(&files)?;
            fs::remove_dir(&files)
                .map_err(|e| io_error("remove incomplete backup directory", &files, e))
        })();
        return match cleanup {
            Ok(()) => Err(failure),
            Err(cleanup) => Err(Error::Cleanup {
                failure: Box::new(failure),
                cleanup: Box::new(cleanup),
            }),
        };
    }
    Ok(())
}

fn journal_metadata(state: &Path) -> Result<Journal, Error> {
    let journal: Journal = read_json(&state.join(JOURNAL))?;
    if journal.version != 1 {
        return Err(Error::InvalidState("unsupported backup version".into()));
    }
    let old = list(&journal.old, false)?;
    let incoming = list(&journal.incoming, false)?;
    let actual: Vec<_> = journal.entries.iter().map(|e| e.path.clone()).collect();
    list(&actual, true)?;
    if actual != union(&old, &incoming)? {
        return Err(Error::InvalidState(
            "backup entries do not match the owned-file union".into(),
        ));
    }
    Ok(journal)
}

fn journal(state: &Path) -> Result<Journal, Error> {
    let journal = journal_metadata(state)?;
    for (index, _) in journal
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.existed)
    {
        if !regular(&state.join("files").join(index.to_string()))? {
            return Err(Error::InvalidState("backup file is missing".into()));
        }
    }
    Ok(journal)
}

fn transaction(install: &Path, state: &Path) -> Result<(PathBuf, PathBuf, Journal), Error> {
    let (install, state) = roots(install, state)?;
    let journal = journal(&state)?;
    if path_key(&install) != path_key(&journal.install) {
        return Err(Error::InvalidState(
            "backup belongs to another install directory".into(),
        ));
    }
    Ok((install, state, journal))
}

fn remove_file(path: &Path) -> Result<(), Error> {
    if regular(path)? {
        fs::remove_file(path).map_err(|e| io_error("remove owned file", path, e))?;
    }
    Ok(())
}

fn prune_parents(root: &Path, file: &Path) -> Result<(), Error> {
    let mut current = file.parent();
    while let Some(path) = current.filter(|p| *p != root && p.starts_with(root)) {
        if inspect(path)?.is_some() {
            match fs::remove_dir(path) {
                Ok(()) => (),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) =>
                {
                    break;
                }
                Err(e) => return Err(io_error("remove empty owned directory", path, e)),
            }
        }
        current = path.parent();
    }
    Ok(())
}

/// Restore original files and remove files which were absent before setup.
/// Keep every backup on failure and success so callers can retry until finalization.
pub fn rollback(install: &Path, state: &Path) -> Result<(), Error> {
    let (install, state, journal) = transaction(install, state)?;
    let pending = install.join(MANIFEST_PENDING);
    regular(&pending)?;
    // Validate every destination before changing any of them.
    for entry in &journal.entries {
        regular(&install.join(&entry.path))?;
    }
    remove_file(&pending)?;
    for (index, entry) in journal.entries.iter().enumerate() {
        let dest = install.join(&entry.path);
        if entry.existed {
            ensure_directory(
                dest.parent()
                    .ok_or_else(|| Error::InvalidPath(entry.path.clone()))?,
            )?;
            regular(&dest)?;
            let source = state.join("files").join(index.to_string());
            regular(&source)?;
            // Replace the directory entry rather than writing through a hard link.
            // Keeping the old destination intact also permits a retry after failure.
            fs::copy(&source, &pending)
                .map_err(|e| io_error("prepare restoration", &pending, e))?;
            regular(&dest)?;
            fs::rename(&pending, &dest).map_err(|e| io_error("restore", &dest, e))?;
        } else {
            remove_file(&dest)?;
            prune_parents(&install, &dest)?;
        }
    }
    Ok(())
}

/// Retire old program files and publish ownership of the installed payload.
/// Call `finalize` only after startup and registry changes also succeed.
pub fn commit(install: &Path, state: &Path, incoming: &[String]) -> Result<(), Error> {
    let incoming = list(incoming, false)?;
    let (install, _, journal) = transaction(install, state)?;
    if incoming != journal.incoming {
        return Err(Error::InvalidState(
            "incoming files differ from the backup".into(),
        ));
    }
    for path in &incoming {
        if !regular(&install.join(path))? {
            return Err(Error::InvalidState(format!(
                "incoming file {path} is missing"
            )));
        }
    }
    let incoming_keys: BTreeSet<_> = incoming.iter().map(|p| p.to_lowercase()).collect();
    let retired: Vec<_> = journal
        .old
        .iter()
        .filter(|p| !incoming_keys.contains(&p.to_lowercase()))
        .collect();
    for path in &retired {
        regular(&install.join(path))?;
    }
    regular(&install.join(MANIFEST))?;
    let pending = install.join(MANIFEST_PENDING);
    if inspect(&pending)?.is_some() {
        return Err(Error::InvalidState(
            "pending manifest already exists".into(),
        ));
    }
    write_new_json(&pending, &incoming)?;
    let result = (|| {
        for path in retired {
            let path = install.join(path);
            remove_file(&path)?;
            prune_parents(&install, &path)?;
        }
        regular(&install.join(MANIFEST))?;
        fs::rename(&pending, install.join(MANIFEST))
            .map_err(|e| io_error("publish manifest", &pending, e))
    })();
    if result.is_err() {
        let _ = remove_file(&pending);
    }
    result
}

/// Remove only this transaction's backup. Additional helper state stays intact.
pub fn finalize(state: &Path) -> Result<(), Error> {
    let state = absolute(state)?;
    if !regular(&state.join(JOURNAL))? {
        if inspect(&state.join("files"))?.is_some() {
            return Err(Error::InvalidState(
                "snapshot files exist without their journal".into(),
            ));
        }
        return Ok(());
    }
    let journal = journal_metadata(&state)?;
    for (index, _) in journal
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.existed)
    {
        remove_file(&state.join("files").join(index.to_string()))?;
    }
    let files = state.join("files");
    if inspect(&files)?.is_some() {
        fs::remove_dir(&files).map_err(|e| io_error("remove empty backup directory", &files, e))?;
    }
    remove_file(&state.join(JOURNAL))
}

/// Read explicit ownership; a missing manifest grants no file ownership.
pub fn owned(install: &Path) -> Result<Vec<String>, Error> {
    let install = absolute(install)?;
    let manifest = install.join(MANIFEST);
    if regular(&manifest)? {
        list(&read_json::<Vec<String>>(&manifest)?, false)
    } else {
        Ok(Vec::new())
    }
}
/// Uninstall owned files only; preserve the install root and unrelated files.
pub fn remove_owned(install: &Path) -> Result<(), Error> {
    let install = absolute(install)?;
    remove_files(&install, &owned(&install)?)
}

/// Finish an interrupted uninstall using its saved exact ownership list.
pub fn remove_files(install: &Path, paths: &[String]) -> Result<(), Error> {
    let install = absolute(install)?;
    let mut paths = list(paths, false)?;
    paths.push(MANIFEST.to_owned());
    for path in &paths {
        regular(&install.join(path))?;
    }
    for path in paths {
        let path = install.join(path);
        remove_file(&path)?;
        prune_parents(&install, &path)?;
    }
    Ok(())
}

const DIRECTORY_STATE: &str = "directory-state.json";
const PREVIOUS_INSTALL: &str = "previous-install";
const FAILED_INSTALL: &str = "failed-install";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryJournal {
    version: u32,
    install: PathBuf,
}

fn archive_root_allowed(root: &Path) -> Result<(), Error> {
    if root.parent().is_none() {
        return Err(Error::InvalidPath(root.display().to_string()));
    }
    let key = path_key(root).trim_end_matches('/').to_owned();
    for variable in [
        "USERPROFILE",
        "HOME",
        "LOCALAPPDATA",
        "APPDATA",
        "WINDIR",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "PUBLIC",
    ] {
        if let Some(value) = std::env::var_os(variable).filter(|p| Path::new(p).is_absolute()) {
            let protected = fs::canonicalize(Path::new(&value))
                .map_err(|e| io_error("resolve protected directory", Path::new(&value), e))?;
            let protected = path_key(&protected).trim_end_matches('/').to_owned();
            if key == protected || protected.starts_with(&format!("{key}/")) {
                return Err(Error::InvalidState(
                    "the registered install directory is a broad system or profile root".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_tree(root: &Path) -> Result<(), Error> {
    let Some(meta) = inspect(root)? else {
        return Ok(());
    };
    if meta.is_dir() {
        for entry in fs::read_dir(root).map_err(|e| io_error("read archived directory", root, e))? {
            validate_tree(
                &entry
                    .map_err(|e| io_error("read archived entry", root, e))?
                    .path(),
            )?;
        }
    } else if !meta.is_file() {
        return Err(Error::InvalidPath(root.display().to_string()));
    }
    Ok(())
}

fn directory_tree(root: &Path) -> Result<bool, Error> {
    match inspect(root)? {
        Some(meta) if meta.is_dir() => {
            validate_tree(root)?;
            Ok(true)
        }
        Some(_) => Err(Error::InvalidState(
            "program archive path is not a directory".into(),
        )),
        None => Ok(false),
    }
}

fn directory_transaction(install: &Path, state: &Path) -> Result<(PathBuf, PathBuf), Error> {
    let (install, state) = roots(install, state)?;
    archive_root_allowed(&install)?;
    let saved: DirectoryJournal = read_json(&state.join(DIRECTORY_STATE))?;
    if saved.version != 1 || path_key(&saved.install) != path_key(&install) {
        return Err(Error::InvalidState(
            "directory archive belongs to another installation or version".into(),
        ));
    }
    Ok((install, state))
}

pub fn directory_archived(install: &Path, state: &Path) -> Result<bool, Error> {
    let (_, state) = directory_transaction(install, state)?;
    match inspect(&state.join(PREVIOUS_INSTALL))? {
        Some(meta) if meta.is_dir() => Ok(true),
        Some(_) => Err(Error::InvalidState(
            "program archive is not a directory".into(),
        )),
        None => Ok(false),
    }
}

pub fn validate_incoming(incoming: &[String]) -> Result<Vec<String>, Error> {
    list(incoming, false)
}

/// Prove that an unpublished helper preparation belongs to this root. The helper
/// publishes process state before pausing startup or allowing any payload writes.
pub fn validate_preparation(install: &Path, state: &Path) -> Result<(), Error> {
    let (install, state) = roots(install, state)?;
    let files = regular(&state.join(JOURNAL))?;
    let directory = regular(&state.join(DIRECTORY_STATE))?;
    if !files && !directory {
        return Err(Error::InvalidState(
            "unpublished preparation has no valid root binding".into(),
        ));
    }
    if files && path_key(&journal_metadata(&state)?.install) != path_key(&install) {
        return Err(Error::InvalidState(
            "file backup belongs to another installation".into(),
        ));
    }
    if directory {
        directory_transaction(&install, &state)?;
        for name in [PREVIOUS_INSTALL, FAILED_INSTALL] {
            if inspect(&state.join(name))?.is_some() {
                return Err(Error::InvalidState(
                    "a program directory was already moved; published process state is required"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

/// Prepare an opaque replacement of a legacy installer-owned program directory.
/// The durable root binding precedes any rename; no provider/user data is inspected.
pub fn prepare_directory(install: &Path, state: &Path) -> Result<(), Error> {
    let (install, state) = roots(install, state)?;
    archive_root_allowed(&install)?;
    if !inspect(&install)?.is_some_and(|m| m.is_dir()) {
        return Err(Error::InvalidState(
            "registered program directory is missing".into(),
        ));
    }
    validate_tree(&install)?;
    for name in [
        DIRECTORY_STATE,
        PREVIOUS_INSTALL,
        FAILED_INSTALL,
        JOURNAL,
        "files",
    ] {
        if inspect(&state.join(name))?.is_some() {
            return Err(Error::InvalidState(
                "unfinished installation backup exists".into(),
            ));
        }
    }
    ensure_directory(&state)?;
    write_new_json(
        &state.join(DIRECTORY_STATE),
        &DirectoryJournal {
            version: 1,
            install,
        },
    )
}

/// Atomically move the stopped legacy program tree aside on the same volume.
pub fn archive_directory(install: &Path, state: &Path) -> Result<(), Error> {
    let (install, state) = directory_transaction(install, state)?;
    let previous = state.join(PREVIOUS_INSTALL);
    if directory_tree(&previous)? {
        return ensure_directory(&install);
    }
    if !inspect(&install)?.is_some_and(|m| m.is_dir()) {
        return Err(Error::InvalidState(
            "program directory disappeared before archive".into(),
        ));
    }
    validate_tree(&install)?;
    fs::rename(&install, &previous)
        .map_err(|e| io_error("archive program directory", &install, e))?;
    // On failure, previous-install still holds the complete original. Rollback also
    // recognizes a crash between this rename and recreation of the install root.
    ensure_directory(&install)
}

/// Restore the entire original tree; preserve the failed replacement until cleanup.
pub fn rollback_directory(install: &Path, state: &Path) -> Result<(), Error> {
    let (install, state) = directory_transaction(install, state)?;
    let previous = state.join(PREVIOUS_INSTALL);
    if !directory_tree(&previous)? {
        return Ok(());
    }
    directory_tree(&install)?;
    let failed = state.join(FAILED_INSTALL);
    if inspect(&install)?.is_some() {
        if inspect(&failed)?.is_some() {
            return Err(Error::InvalidState(
                "failed replacement already exists; refusing to overwrite it".into(),
            ));
        }
        fs::rename(&install, &failed)
            .map_err(|e| io_error("retain failed replacement", &install, e))?;
    }
    fs::rename(&previous, &install).map_err(|e| io_error("restore program directory", &install, e))
}

/// Publish only the new payload's manifest, keeping the archive until full success.
pub fn commit_directory(install: &Path, state: &Path, incoming: &[String]) -> Result<(), Error> {
    let incoming = list(incoming, false)?;
    let (install, state) = directory_transaction(install, state)?;
    if !directory_tree(&state.join(PREVIOUS_INSTALL))? {
        return Err(Error::InvalidState(
            "legacy program directory was not archived".into(),
        ));
    }
    for path in &incoming {
        if !regular(&install.join(path))? {
            return Err(Error::InvalidState(format!(
                "incoming file {path} is missing"
            )));
        }
    }
    regular(&install.join(MANIFEST))?;
    let pending = install.join(MANIFEST_PENDING);
    write_new_json(&pending, &incoming)?;
    fs::rename(&pending, install.join(MANIFEST))
        .map_err(|e| io_error("publish manifest", &pending, e))
}

fn remove_tree(root: &Path) -> Result<(), Error> {
    let Some(meta) = inspect(root)? else {
        return Ok(());
    };
    if meta.is_dir() {
        for entry in fs::read_dir(root).map_err(|e| io_error("read owned archive", root, e))? {
            remove_tree(
                &entry
                    .map_err(|e| io_error("read owned archive entry", root, e))?
                    .path(),
            )?;
        }
        fs::remove_dir(root).map_err(|e| io_error("remove empty archive directory", root, e))
    } else if meta.is_file() {
        #[cfg(windows)]
        #[allow(
            clippy::permissions_set_readonly_false,
            reason = "Windows clears FILE_ATTRIBUTE_READONLY without changing the file's ACL"
        )]
        if meta.permissions().readonly() {
            let mut permissions = meta.permissions();
            permissions.set_readonly(false);
            fs::set_permissions(root, permissions)
                .map_err(|e| io_error("make owned archive writable", root, e))?;
        }
        fs::remove_file(root).map_err(|e| io_error("remove owned archive file", root, e))
    } else {
        Err(Error::InvalidPath(root.display().to_string()))
    }
}

/// Cleanup only the journal-bound previous and failed program trees, never state extras.
pub fn finalize_directory(state: &Path) -> Result<(), Error> {
    let state = absolute(state)?;
    if !regular(&state.join(DIRECTORY_STATE))? {
        for name in [PREVIOUS_INSTALL, FAILED_INSTALL] {
            if inspect(&state.join(name))?.is_some() {
                return Err(Error::InvalidState(
                    "archive exists without its root binding".into(),
                ));
            }
        }
        return Ok(());
    }
    let saved: DirectoryJournal = read_json(&state.join(DIRECTORY_STATE))?;
    let (_, state) = directory_transaction(&saved.install, &state)?;
    for name in [PREVIOUS_INSTALL, FAILED_INSTALL] {
        directory_tree(&state.join(name))?;
    }
    for name in [PREVIOUS_INSTALL, FAILED_INSTALL] {
        remove_tree(&state.join(name))?;
    }
    remove_file(&state.join(DIRECTORY_STATE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tidemark-files-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::create_dir(path.join("install")).unwrap();
            Self(path)
        }
        fn install(&self) -> PathBuf {
            self.0.join("install")
        }
        fn state(&self) -> PathBuf {
            self.0.join("state")
        }
        fn put(&self, path: &str, bytes: &[u8]) {
            let path = self.install().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
        fn manifest(&self, paths: &[&str]) {
            self.put("install-manifest.json", &serde_json::to_vec(paths).unwrap());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn paths(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn directory_transaction_restores_the_opaque_original_after_replacement() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"old program");
        f.put("extra/unknown.txt", b"old opaque content");
        prepare_directory(&f.install(), &f.state()).unwrap();
        archive_directory(&f.install(), &f.state()).unwrap();
        assert_eq!(fs::read_dir(f.install()).unwrap().count(), 0);
        f.put("tidemark.exe", b"new program");
        commit_directory(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        assert_eq!(owned(&f.install()).unwrap(), paths(&["tidemark.exe"]));
        rollback_directory(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"old program"
        );
        assert_eq!(
            fs::read(f.install().join("extra/unknown.txt")).unwrap(),
            b"old opaque content"
        );
        assert!(!f.install().join(MANIFEST).exists());
        finalize_directory(&f.state()).unwrap();
        assert!(!f.state().join("directory-state.json").exists());
    }

    #[test]
    fn interrupted_archive_can_restore_after_the_rename_before_root_recreation() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        prepare_directory(&f.install(), &f.state()).unwrap();
        fs::rename(f.install(), f.state().join("previous-install")).unwrap();
        rollback_directory(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn directory_archive_and_cleanup_reject_links_and_other_roots() {
        let f = Fixture::new();
        let other = Fixture::new();
        f.put("tidemark.exe", b"original");
        prepare_directory(&f.install(), &f.state()).unwrap();
        assert!(archive_directory(&other.install(), &f.state()).is_err());
        directory_link(&other.install(), &f.install().join("linked"));
        assert!(archive_directory(&f.install(), &f.state()).is_err());
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn directory_commit_and_finalize_keep_the_replacement() {
        let f = Fixture::new();
        f.put("old.dll", b"old");
        prepare_directory(&f.install(), &f.state()).unwrap();
        archive_directory(&f.install(), &f.state()).unwrap();
        f.put("tidemark.exe", b"new");
        commit_directory(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        finalize_directory(&f.state()).unwrap();
        assert_eq!(fs::read(f.install().join("tidemark.exe")).unwrap(), b"new");
        assert!(!f.state().join("previous-install").exists());
        assert!(!f.install().join("old.dll").exists());
    }

    #[test]
    fn cancellation_before_archive_keeps_the_original_directory() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        prepare_directory(&f.install(), &f.state()).unwrap();
        rollback_directory(&f.install(), &f.state()).unwrap();
        finalize_directory(&f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn directory_cleanup_preflights_links_before_deleting_any_archive_files() {
        let f = Fixture::new();
        let other = Fixture::new();
        f.put("tidemark.exe", b"original");
        prepare_directory(&f.install(), &f.state()).unwrap();
        archive_directory(&f.install(), &f.state()).unwrap();
        let link = f.state().join(PREVIOUS_INSTALL).join("linked");
        directory_link(&other.install(), &link);
        assert!(finalize_directory(&f.state()).is_err());
        assert_eq!(
            fs::read(f.state().join(PREVIOUS_INSTALL).join("tidemark.exe")).unwrap(),
            b"original"
        );
        assert!(f.state().join(DIRECTORY_STATE).exists());
        // A junction is removed as a directory; a unix symlink is removed as a file.
        #[cfg(windows)]
        fs::remove_dir(link).unwrap();
        #[cfg(unix)]
        fs::remove_file(link).unwrap();
        finalize_directory(&f.state()).unwrap();
    }

    #[test]
    fn failed_directory_restore_retains_original_and_can_retry() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        prepare_directory(&f.install(), &f.state()).unwrap();
        archive_directory(&f.install(), &f.state()).unwrap();
        f.put("tidemark.exe", b"replacement");
        fs::create_dir(f.state().join(FAILED_INSTALL)).unwrap();
        assert!(rollback_directory(&f.install(), &f.state()).is_err());
        assert_eq!(
            fs::read(f.state().join(PREVIOUS_INSTALL).join("tidemark.exe")).unwrap(),
            b"original"
        );
        fs::remove_dir(f.state().join(FAILED_INSTALL)).unwrap();
        rollback_directory(&f.install(), &f.state()).unwrap();
        rollback_directory(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
        fs::write(f.state().join("unrelated.txt"), b"preserve").unwrap();
        finalize_directory(&f.state()).unwrap();
        finalize_directory(&f.state()).unwrap();
        assert_eq!(
            fs::read(f.state().join("unrelated.txt")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn directory_transaction_rejects_volume_and_profile_roots() {
        let f = Fixture::new();
        let root = f.install().canonicalize().unwrap();
        let volume = root.ancestors().last().unwrap();
        assert!(archive_root_allowed(volume).is_err());
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            assert!(archive_root_allowed(&fs::canonicalize(profile).unwrap()).is_err());
        }
    }

    #[test]
    fn archive_cleanup_rejects_regular_file_collisions() {
        let f = Fixture::new();
        prepare_directory(&f.install(), &f.state()).unwrap();
        fs::write(f.state().join(PREVIOUS_INSTALL), b"unrelated collision").unwrap();
        assert!(archive_directory(&f.install(), &f.state()).is_err());
        assert!(finalize_directory(&f.state()).is_err());
        assert_eq!(
            fs::read(f.state().join(PREVIOUS_INSTALL)).unwrap(),
            b"unrelated collision"
        );
    }

    #[test]
    fn a_missing_manifest_never_claims_existing_program_files() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"untracked");
        f.put("unknown.txt", b"personal");
        assert!(owned(&f.install()).unwrap().is_empty());
        remove_owned(&f.install()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"untracked"
        );
        assert_eq!(
            fs::read(f.install().join("unknown.txt")).unwrap(),
            b"personal"
        );
    }

    #[test]
    fn rollback_restores_replaced_deleted_and_new_files_and_manifest() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"old exe");
        f.put("libsqlite3-0.dll", b"old dll");
        f.put("notes.txt", b"personal");
        f.manifest(&["tidemark.exe", "libsqlite3-0.dll"]);
        let original_manifest = fs::read(f.install().join("install-manifest.json")).unwrap();
        let incoming = paths(&["tidemark.exe", "share/tidemark.ico"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        f.put("tidemark.exe", b"new exe");
        f.put("share/tidemark.ico", b"new icon");
        fs::remove_file(f.install().join("libsqlite3-0.dll")).unwrap();
        f.manifest(&["tidemark.exe", "share/tidemark.ico"]);
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"old exe"
        );
        assert_eq!(
            fs::read(f.install().join("libsqlite3-0.dll")).unwrap(),
            b"old dll"
        );
        assert!(!f.install().join("share/tidemark.ico").exists());
        assert_eq!(
            fs::read(f.install().join("notes.txt")).unwrap(),
            b"personal"
        );
        assert_eq!(
            fs::read(f.install().join("install-manifest.json")).unwrap(),
            original_manifest
        );
    }

    #[test]
    fn commit_only_retires_owned_files_and_keeps_rollback_until_finalize() {
        let f = Fixture::new();
        f.put("old.dll", b"retired");
        f.put("notes.txt", b"personal");
        f.manifest(&["old.dll"]);
        let incoming = paths(&["tidemark.exe"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        f.put("tidemark.exe", b"new");
        commit(&f.install(), &f.state(), &incoming).unwrap();
        assert!(!f.install().join("old.dll").exists());
        assert_eq!(owned(&f.install()).unwrap(), incoming);
        assert_eq!(
            fs::read(f.install().join("notes.txt")).unwrap(),
            b"personal"
        );
        assert!(f.state().join("journal.json").exists());
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(fs::read(f.install().join("old.dll")).unwrap(), b"retired");
        assert!(!f.install().join("tidemark.exe").exists());
        finalize(&f.state()).unwrap();
        assert!(!f.state().join("journal.json").exists());
    }

    #[test]
    fn unsafe_and_case_duplicate_paths_fail_before_mutation() {
        for incoming in [
            paths(&["../victim"]),
            paths(&["C:/victim"]),
            paths(&["\\\\server\\victim"]),
            paths(&["/victim"]),
            paths(&["file:stream"]),
            paths(&["share/./a"]),
            paths(&["app.exe", "APP.exe"]),
            paths(&["app.exe", "app.exe/child"]),
            paths(&["trailing."]),
            paths(&["NUL"]),
        ] {
            let f = Fixture::new();
            f.put("tidemark.exe", b"original");
            assert!(
                backup(&f.install(), &f.state(), &incoming).is_err(),
                "accepted {incoming:?}"
            );
            assert_eq!(
                fs::read(f.install().join("tidemark.exe")).unwrap(),
                b"original"
            );
            assert!(!f.state().join("journal.json").exists());
        }
    }

    #[test]
    fn malformed_manifest_and_duplicate_manifest_fail_closed() {
        for manifest in [
            b"not json".as_slice(),
            b"[\"../victim\"]",
            b"[\"file\",\"FILE\"]",
        ] {
            let f = Fixture::new();
            f.put("install-manifest.json", manifest);
            assert!(backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).is_err());
            assert!(remove_owned(&f.install()).is_err());
            assert_eq!(
                fs::read(f.install().join("install-manifest.json")).unwrap(),
                manifest
            );
        }
    }

    #[test]
    fn rollback_rejects_another_install_root_and_overlapping_state() {
        let f = Fixture::new();
        let other = Fixture::new();
        backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        other.put("tidemark.exe", b"untouched");
        assert!(rollback(&other.install(), &f.state()).is_err());
        assert!(backup(&f.install(), &f.install().join("state"), &[]).is_err());
        assert!(backup(&f.install(), &f.0, &[]).is_err());
        assert_eq!(
            fs::read(other.install().join("tidemark.exe")).unwrap(),
            b"untouched"
        );
    }

    #[test]
    fn failed_restore_keeps_backup_for_retry() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        fs::remove_file(f.install().join("tidemark.exe")).unwrap();
        fs::create_dir(f.install().join("tidemark.exe")).unwrap();
        f.put("tidemark.exe/personal.txt", b"must survive");
        assert!(rollback(&f.install(), &f.state()).is_err());
        assert!(f.state().join("journal.json").exists());
        assert_eq!(
            fs::read(f.install().join("tidemark.exe/personal.txt")).unwrap(),
            b"must survive"
        );
        fs::remove_file(f.install().join("tidemark.exe/personal.txt")).unwrap();
        fs::remove_dir(f.install().join("tidemark.exe")).unwrap();
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn manifest_cleanup_preserves_unknown_assets_and_dlls() {
        let f = Fixture::new();
        f.manifest(&[
            "tidemark.exe",
            "tidemarkd.exe",
            "uninstall.exe",
            "libgcc_s_seh-1.dll",
            "libsqlite3-0.dll",
            "share/tidemark.ico",
            "share/icons/hicolor/symbolic/apps/tidemark-claude-symbolic.svg",
        ]);
        for path in [
            "tidemark.exe",
            "tidemarkd.exe",
            "uninstall.exe",
            "libgcc_s_seh-1.dll",
            "libsqlite3-0.dll",
            "share/tidemark.ico",
            "share/icons/hicolor/symbolic/apps/tidemark-claude-symbolic.svg",
        ] {
            f.put(path, b"owned");
        }
        for path in [
            "notes.txt",
            "my-plugin.dll",
            "share/docs/my.txt",
            "etc/custom.conf",
            "lib/custom.dll",
            "share/icons/hicolor/symbolic/apps/mine.svg",
            "share/icons/hicolor/symbolic/apps/tidemark-personal-symbolic.svg",
        ] {
            f.put(path, b"personal");
        }
        remove_owned(&f.install()).unwrap();
        assert!(!f.install().join("tidemark.exe").exists());
        assert!(!f.install().join("libgcc_s_seh-1.dll").exists());
        assert!(
            !f.install()
                .join("share/icons/hicolor/symbolic/apps/tidemark-claude-symbolic.svg")
                .exists()
        );
        for path in [
            "notes.txt",
            "my-plugin.dll",
            "share/docs/my.txt",
            "etc/custom.conf",
            "lib/custom.dll",
            "share/icons/hicolor/symbolic/apps/mine.svg",
            "share/icons/hicolor/symbolic/apps/tidemark-personal-symbolic.svg",
        ] {
            assert_eq!(
                fs::read(f.install().join(path)).unwrap(),
                b"personal",
                "{path}"
            );
        }
    }

    #[test]
    fn saved_uninstall_list_removes_files_after_the_manifest_is_gone() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"program");
        f.put("share/tidemark.ico", b"icon");
        f.put("notes.txt", b"personal");
        remove_files(
            &f.install(),
            &paths(&["tidemark.exe", "share/tidemark.ico"]),
        )
        .unwrap();
        assert!(!f.install().join("tidemark.exe").exists());
        assert!(!f.install().join("share/tidemark.ico").exists());
        assert_eq!(
            fs::read(f.install().join("notes.txt")).unwrap(),
            b"personal"
        );
        assert!(f.install().exists());
    }

    #[test]
    fn malicious_saved_uninstall_list_is_rejected_before_removing_valid_files() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"program");
        assert!(remove_files(&f.install(), &paths(&["tidemark.exe", "../personal.txt"])).is_err());
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"program"
        );
    }

    #[test]
    fn finalization_can_retry_when_a_snapshot_was_already_removed() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        // A prior finalization removed this file before later cleanup failed.
        fs::remove_file(f.state().join("files/0")).unwrap();
        finalize(&f.state()).unwrap();
        assert!(!f.state().join(JOURNAL).exists());
        assert!(!f.state().join("files").exists());
    }

    #[test]
    fn a_previous_backup_is_never_overwritten() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        let incoming = paths(&["tidemark.exe"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        f.put("tidemark.exe", b"replacement");
        assert!(backup(&f.install(), &f.state(), &incoming).is_err());
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn commit_rejects_unbacked_and_missing_incoming_before_retiring_files() {
        let f = Fixture::new();
        f.put("old.dll", b"original");
        f.manifest(&["old.dll"]);
        let incoming = paths(&["tidemark.exe"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        assert!(commit(&f.install(), &f.state(), &paths(&["another.exe"])).is_err());
        assert!(commit(&f.install(), &f.state(), &incoming).is_err());
        assert_eq!(fs::read(f.install().join("old.dll")).unwrap(), b"original");
        assert_eq!(owned(&f.install()).unwrap(), paths(&["old.dll"]));
    }

    #[test]
    fn finalize_preserves_other_helper_state() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        fs::write(f.state().join("process-state.json"), b"processes").unwrap();
        finalize(&f.state()).unwrap();
        assert_eq!(
            fs::read(f.state().join("process-state.json")).unwrap(),
            b"processes"
        );
        assert!(!f.state().join("files").exists());
    }

    #[test]
    fn fresh_install_rollback_removes_all_new_program_files() {
        let f = Fixture::new();
        fs::remove_dir(f.install()).unwrap();
        let incoming = paths(&["tidemark.exe", "share/tidemark.ico"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        f.put("tidemark.exe", b"new");
        f.put("share/tidemark.ico", b"icon");
        commit(&f.install(), &f.state(), &incoming).unwrap();
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(fs::read_dir(f.install()).unwrap().count(), 0);
    }

    #[test]
    fn case_aliases_between_old_and_incoming_fail_closed() {
        let f = Fixture::new();
        f.put("Tidemark.exe", b"original");
        f.manifest(&["Tidemark.exe"]);
        assert!(backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).is_err());
        assert!(!f.state().join(JOURNAL).exists());
    }

    fn directory_link(source: &Path, link: &Path) {
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(link)
                .arg(source)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "could not create temporary junction");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, link).unwrap();
    }

    #[test]
    fn linked_ancestors_leaf_and_backup_root_are_rejected() {
        let f = Fixture::new();
        let target = f.0.join("other");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("outside.txt"), b"personal").unwrap();
        directory_link(&target, &f.install().join("share"));
        assert!(backup(&f.install(), &f.state(), &paths(&["share/outside.txt"])).is_err());
        directory_link(&target, &f.install().join("tidemark.exe"));
        assert!(backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).is_err());
        directory_link(&target, &f.state());
        assert!(backup(&f.install(), &f.state(), &[]).is_err());
        assert_eq!(fs::read(target.join("outside.txt")).unwrap(), b"personal");
    }

    #[test]
    fn rollback_rejects_a_destination_replaced_by_a_link() {
        let f = Fixture::new();
        f.put("share/tidemark.ico", b"original");
        backup(&f.install(), &f.state(), &paths(&["share/tidemark.ico"])).unwrap();
        fs::remove_file(f.install().join("share/tidemark.ico")).unwrap();
        fs::remove_dir(f.install().join("share")).unwrap();
        let target = f.0.join("other");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("tidemark.ico"), b"personal").unwrap();
        directory_link(&target, &f.install().join("share"));
        assert!(rollback(&f.install(), &f.state()).is_err());
        assert!(f.state().join(JOURNAL).exists());
        assert_eq!(fs::read(target.join("tidemark.ico")).unwrap(), b"personal");
    }

    #[test]
    fn rollback_replaces_hard_link_without_writing_the_unowned_target() {
        let f = Fixture::new();
        f.put("tidemark.exe", b"original");
        backup(&f.install(), &f.state(), &paths(&["tidemark.exe"])).unwrap();
        fs::remove_file(f.install().join("tidemark.exe")).unwrap();
        let outside = f.0.join("personal.txt");
        fs::write(&outside, b"personal").unwrap();
        fs::hard_link(&outside, f.install().join("tidemark.exe")).unwrap();
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(fs::read(outside).unwrap(), b"personal");
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn rollback_removes_a_manifest_left_pending_by_interrupted_commit() {
        let f = Fixture::new();
        backup(&f.install(), &f.state(), &[]).unwrap();
        f.put(MANIFEST_PENDING, b"pending manifest");
        rollback(&f.install(), &f.state()).unwrap();
        assert!(!f.install().join(MANIFEST_PENDING).exists());
    }

    #[test]
    fn an_existing_pending_manifest_is_not_claimed_or_removed() {
        let f = Fixture::new();
        f.put(MANIFEST_PENDING, b"unowned");
        assert!(backup(&f.install(), &f.state(), &[]).is_err());
        assert_eq!(
            fs::read(f.install().join(MANIFEST_PENDING)).unwrap(),
            b"unowned"
        );
    }

    #[cfg(windows)]
    #[test]
    fn locked_file_restore_failure_keeps_originals_and_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        f.put("tidemark.exe", b"original gui");
        f.put("tidemarkd.exe", b"original daemon");
        let incoming = paths(&["tidemark.exe", "tidemarkd.exe"]);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        f.put("tidemark.exe", b"replacement gui");
        f.put("tidemarkd.exe", b"replacement daemon");
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(f.install().join("tidemarkd.exe"))
            .unwrap();
        assert!(rollback(&f.install(), &f.state()).is_err());
        assert!(f.state().join(JOURNAL).exists());
        drop(lock);
        rollback(&f.install(), &f.state()).unwrap();
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original gui"
        );
        assert_eq!(
            fs::read(f.install().join("tidemarkd.exe")).unwrap(),
            b"original daemon"
        );
    }

    #[cfg(windows)]
    #[test]
    fn failed_backup_removes_only_new_snapshot_files_and_can_retry() {
        use std::os::windows::fs::OpenOptionsExt;
        let f = Fixture::new();
        f.put("tidemark.exe", b"original gui");
        f.put("tidemarkd.exe", b"original daemon");
        fs::create_dir(f.state()).unwrap();
        fs::write(
            f.state().join("process-state.json"),
            b"existing helper state",
        )
        .unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(f.install().join("tidemarkd.exe"))
            .unwrap();
        let incoming = paths(&["tidemark.exe", "tidemarkd.exe"]);
        assert!(backup(&f.install(), &f.state(), &incoming).is_err());
        assert!(!f.state().join("files").exists());
        assert!(!f.state().join(JOURNAL).exists());
        assert_eq!(
            fs::read(f.state().join("process-state.json")).unwrap(),
            b"existing helper state"
        );
        assert_eq!(
            fs::read(f.install().join("tidemark.exe")).unwrap(),
            b"original gui"
        );
        drop(lock);
        backup(&f.install(), &f.state(), &incoming).unwrap();
        rollback(&f.install(), &f.state()).unwrap();
    }
}
