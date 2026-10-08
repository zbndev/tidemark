#![allow(unsafe_code)]
//! Preserve the exact per-user shell integration that Inno's rollback would delete.
//! RegCopyTree keeps raw value types/bytes and child keys without registry privileges.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, RegCloseKey, RegCopyTreeW, RegCreateKeyExW,
    RegDeleteKeyExW, RegDeleteTreeW, RegOpenKeyExW,
};
use windows::core::{HSTRING, PCWSTR};

const META: &str = "integration-state.json";
const LINK: &str = "start-menu-link.bak";
const PREFIX: &str = r"Software\io.github.zbndev.Tidemark\SetupRecovery\";
const KEYS: [(&str, REG_SAM_FLAGS); 4] = [
    (
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.zbndev.Tidemark_is1",
        KEY_WOW64_64KEY,
    ),
    (
        r"Software\Classes\AppUserModelId\io.github.zbndev.Tidemark",
        KEY_WOW64_64KEY,
    ),
    (
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.zbndev.Tidemark",
        KEY_WOW64_32KEY,
    ),
    (
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.zbndev.Tidemark",
        KEY_WOW64_64KEY,
    ),
];

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: this RAII owner is the only owner of the open registry handle.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

fn open(path: &str) -> io::Result<Option<Key>> {
    open_view(path, KEY_WOW64_64KEY)
}

fn open_view(path: &str, view: REG_SAM_FLAGS) -> io::Result<Option<Key>> {
    let mut key = HKEY::default();
    // SAFETY: fixed HKCU root, NUL-terminated name, live output handle; no mutation.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            None,
            KEY_READ | view,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status)?;
    Ok(Some(Key(key)))
}

fn create(path: &str) -> io::Result<Key> {
    create_view(path, KEY_WOW64_64KEY)
}

fn create_view(path: &str, view: REG_SAM_FLAGS) -> io::Result<Key> {
    let mut key = HKEY::default();
    // SAFETY: per-user key, non-inherited output handle, default security.
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS | view,
            None,
            &mut key,
            None,
        )
    })?;
    Ok(Key(key))
}

fn remove_registry(path: &str) -> io::Result<()> {
    remove_view(path, KEY_WOW64_64KEY)
}

fn remove_view(path: &str, view: REG_SAM_FLAGS) -> io::Result<()> {
    let mut key = HKEY::default();
    // SAFETY: open the exact owned branch with its original registry view.
    let status = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(path),
            None,
            KEY_ALL_ACCESS | view,
            &mut key,
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status)?;
    let key = Key(key);
    // SAFETY: callers supply only fixed owned keys or a validated recovery key.
    check(unsafe { RegDeleteTreeW(key.0, PCWSTR::null()) })?;
    drop(key);
    // SAFETY: contents were removed through the correct view; remove the empty root.
    check(unsafe { RegDeleteKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), view.0, None) })
}

fn copy(source: &Key, destination: &Key) -> io::Result<()> {
    // SAFETY: both handles are live; a NULL subkey copies the entire source branch.
    check(unsafe { RegCopyTreeW(source.0, PCWSTR::null(), destination.0) })
}

fn regular(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_attributes() & 0x400 != 0 || !meta.is_file() => Err(
            io::Error::other("integration backup contains a link or non-file"),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    state: PathBuf,
    registry: String,
    present: Vec<bool>,
    shortcut: PathBuf,
    shortcut_present: bool,
}

fn read(state: &Path) -> io::Result<Option<Snapshot>> {
    if !regular(&state.join(META))? {
        return Ok(None);
    }
    let saved: Snapshot =
        serde_json::from_slice(&fs::read(state.join(META))?).map_err(io::Error::other)?;
    let suffix = saved
        .registry
        .strip_prefix(PREFIX)
        .ok_or_else(|| io::Error::other("invalid registry backup"))?;
    if saved.state != state.canonicalize()?
        || saved.present.len() != KEYS.len()
        || suffix.is_empty()
        || !suffix.chars().all(|c| c.is_ascii_digit() || c == '-')
        || !saved.shortcut.is_absolute()
        || saved
            .shortcut
            .file_name()
            .is_none_or(|name| name != "Tidemark.lnk")
    {
        return Err(io::Error::other(
            "integration snapshot belongs to another transaction",
        ));
    }
    Ok(Some(saved))
}

/// Called after the file transaction exists, before Inno starts writing files.
pub fn backup(state: &Path, shortcut: &Path) -> io::Result<()> {
    if read(state)?.is_some() {
        return Ok(());
    }
    if !shortcut.is_absolute()
        || shortcut
            .file_name()
            .is_none_or(|name| name != "Tidemark.lnk")
    {
        return Err(io::Error::other("invalid Start-menu shortcut path"));
    }
    if regular(&state.join(LINK))? {
        return Err(io::Error::other(
            "unowned integration backup already exists",
        ));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let registry = format!("{PREFIX}{}-{stamp}", std::process::id());
    let mut saved = Snapshot {
        state: state.canonicalize()?,
        registry: registry.clone(),
        present: Vec::new(),
        shortcut: shortcut.to_owned(),
        shortcut_present: regular(shortcut)?,
    };
    let result = (|| {
        if open(&registry)?.is_some() {
            return Err(io::Error::other("registry backup already exists"));
        }
        let _root = create(&registry)?;
        saved.present = capture_registry(&registry, &KEYS)?;
        if saved.shortcut_present {
            fs::copy(shortcut, state.join(LINK))?;
        }
        let metadata = serde_json::to_vec(&saved).map_err(io::Error::other)?;
        let temporary = state.join("integration-state.pending");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let publication = (|| {
            file.write_all(&metadata)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, state.join(META))
        })();
        if publication.is_err() {
            fs::remove_file(&temporary)?;
        }
        publication?;
        Ok(())
    })();
    if result.is_err() {
        // No target was changed during backup. Remove only this call's own snapshots.
        remove_registry(&registry)?;
        if regular(&state.join(LINK))? {
            fs::remove_file(state.join(LINK))?;
        }
    }
    result
}

/// Idempotent: backups stay available if any restore fails and can be retried.
pub fn rollback(state: &Path) -> io::Result<()> {
    let Some(saved) = read(state)? else {
        return Ok(());
    };
    restore_registry(&saved.registry, &KEYS, &saved.present)?;
    if saved.shortcut_present {
        if !regular(&state.join(LINK))? {
            return Err(io::Error::other("shortcut backup is missing"));
        }
        if regular(&saved.shortcut)? {
            fs::remove_file(&saved.shortcut)?;
        }
        fs::create_dir_all(
            saved
                .shortcut
                .parent()
                .ok_or_else(|| io::Error::other("invalid shortcut"))?,
        )?;
        fs::copy(state.join(LINK), &saved.shortcut)?;
    } else if regular(&saved.shortcut)? {
        fs::remove_file(&saved.shortcut)?;
    }
    Ok(())
}

fn capture_registry(registry: &str, keys: &[(&str, REG_SAM_FLAGS)]) -> io::Result<Vec<bool>> {
    let mut present = Vec::new();
    for (index, (path, view)) in keys.iter().enumerate() {
        if let Some(source) = open_view(path, *view)? {
            copy(&source, &create(&format!("{registry}\\{index}"))?)?;
            present.push(true);
        } else {
            present.push(false);
        }
    }
    Ok(present)
}

fn restore_registry(
    registry: &str,
    keys: &[(&str, REG_SAM_FLAGS)],
    present: &[bool],
) -> io::Result<()> {
    if keys.len() != present.len() {
        return Err(io::Error::other("registry snapshot length mismatch"));
    }
    for (index, (path, view)) in keys.iter().enumerate() {
        // Open the backup before deleting a target: a missing snapshot must fail closed.
        let source = if present[index] {
            Some(
                open(&format!("{registry}\\{index}"))?
                    .ok_or_else(|| io::Error::other("registry backup is missing"))?,
            )
        } else {
            None
        };
        remove_view(path, *view)?;
        if let Some(source) = source {
            copy(&source, &create_view(path, *view)?)?;
        }
    }
    Ok(())
}

/// Cleanup only after file/integration restoration or commit and restart succeeded.
pub fn finalize(state: &Path) -> io::Result<()> {
    let Some(saved) = read(state)? else {
        return Ok(());
    };
    remove_registry(&saved.registry)?;
    if regular(&state.join(LINK))? {
        fs::remove_file(state.join(LINK))?;
    }
    fs::remove_file(state.join(META))
}

/// Complete a failed Inno removal from a helper outside the program directory.
pub fn remove(shortcut: &Path) -> io::Result<()> {
    if !shortcut.is_absolute()
        || shortcut
            .file_name()
            .is_none_or(|name| name != "Tidemark.lnk")
    {
        return Err(io::Error::other("invalid Start-menu shortcut path"));
    }
    for (path, view) in &KEYS[..2] {
        remove_view(path, *view)?;
    }
    if regular(shortcut)? {
        fs::remove_file(shortcut)?;
    }
    // Empty-only removal preserves any unrelated shortcuts in this group.
    if let Some(group) = shortcut.parent() {
        match fs::remove_dir(group) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows::Win32::System::Registry::{
        REG_BINARY, REG_VALUE_TYPE, RegQueryValueExW, RegSetValueExW,
    };

    struct Scratch(String);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(format!(
                r"Software\Tidemark.IntegrationTest.{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
        }
        fn branch(&self, name: &str) -> String {
            format!("{}\\{name}", self.0)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            remove_registry(&self.0).unwrap();
        }
    }
    fn put(key: &Key, bytes: &[u8]) {
        // SAFETY: live scratch key, raw byte slice length supplied by the wrapper.
        check(unsafe {
            RegSetValueExW(key.0, &HSTRING::from("Raw"), None, REG_BINARY, Some(bytes))
        })
        .unwrap();
    }
    fn get(key: &Key) -> (REG_VALUE_TYPE, Vec<u8>) {
        let mut kind = REG_VALUE_TYPE::default();
        let mut bytes = [0u8; 64];
        let mut length = bytes.len() as u32;
        // SAFETY: initialized type/length and writable buffer; scratch keys only.
        check(unsafe {
            RegQueryValueExW(
                key.0,
                &HSTRING::from("Raw"),
                None,
                Some(&mut kind),
                Some(bytes.as_mut_ptr()),
                Some(&mut length),
            )
        })
        .unwrap();
        (kind, bytes[..length as usize].to_vec())
    }

    #[test]
    fn registry_rollback_preserves_raw_nested_values_and_original_absence() {
        let scratch = Scratch::new();
        let target = scratch.branch("target");
        let absent = scratch.branch("absent");
        let nested = format!("{target}\\child");
        let backup = scratch.branch("backup");
        let keys = [
            (target.as_str(), KEY_WOW64_64KEY),
            (absent.as_str(), KEY_WOW64_32KEY),
        ];
        let bytes = [0, 255, 0, 17, 128];
        put(&create(&nested).unwrap(), &bytes);
        let present = capture_registry(&backup, &keys).unwrap();
        assert_eq!(present, [true, false]);
        remove_registry(&target).unwrap();
        put(&create(&format!("{target}\\unexpected")).unwrap(), b"new");
        put(&create_view(&absent, KEY_WOW64_32KEY).unwrap(), b"fresh");
        restore_registry(&backup, &keys, &present).unwrap();
        assert_eq!(
            get(&open(&nested).unwrap().unwrap()),
            (REG_BINARY, bytes.to_vec())
        );
        assert!(open(&format!("{target}\\unexpected")).unwrap().is_none());
        assert!(open_view(&absent, KEY_WOW64_32KEY).unwrap().is_none());
        // A second recovery is allowed after a failed later step.
        restore_registry(&backup, &keys, &present).unwrap();
        assert_eq!(get(&open(&nested).unwrap().unwrap()).1, bytes);
    }

    #[test]
    fn missing_registry_backup_never_deletes_the_existing_target() {
        let scratch = Scratch::new();
        let target = scratch.branch("target");
        let backup = scratch.branch("backup");
        let keys = [(target.as_str(), KEY_WOW64_64KEY)];
        put(&create(&target).unwrap(), b"original");
        let present = capture_registry(&backup, &keys).unwrap();
        remove_registry(&format!("{backup}\\0")).unwrap();
        put(&create(&target).unwrap(), b"current");
        assert!(restore_registry(&backup, &keys, &present).is_err());
        assert_eq!(get(&open(&target).unwrap().unwrap()).1, b"current");
    }
}
