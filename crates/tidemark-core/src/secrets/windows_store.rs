#![allow(unsafe_code)]
//! Windows Credential Manager backend.
//!
//! This is the only workspace module allowed to contain unsafe Rust. Its public surface is
//! entirely safe; raw pointers and Win32-owned allocations never leave `SystemBackend`.

use super::protocol::{
    self, Attributes, Backend, ReadError, Record, STORAGE_ATTRIBUTE, STORAGE_VALUE,
};
use super::{Kind, MutationMap, SecretError};
use crate::providers::Credential;
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex};
use tidemark_types::{AccountId, ProviderId};
use tokio::sync::Mutex;
use windows::Win32::Foundation::{ERROR_NOT_FOUND, HLOCAL, LocalFree};
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIAL_ATTRIBUTEW, CREDENTIALW, CredDeleteW,
    CredFree, CredReadW, CredWriteW,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows::core::{Error as WindowsError, HRESULT, PCWSTR, PWSTR};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// A per-user Windows Credential Manager store.
#[derive(Debug)]
pub struct Store {
    root: PathBuf,
    mutations: MutationMap,
}

impl Store {
    /// Opens the local fallback directory and applies a user/SYSTEM-only DACL.
    pub async fn connect() -> Result<Self, SecretError> {
        let root = fallback_root()?;
        let checked = root.clone();
        tokio::task::spawn_blocking(move || secure_directory(&checked))
            .await
            .map_err(join_error)??;
        Ok(Self {
            root,
            mutations: SyncMutex::new(std::collections::HashMap::new()),
        })
    }

    fn mutation(&self, kind: Kind, provider: &ProviderId, account: &AccountId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.mutations
                .lock()
                .expect("no code panics while holding the credential mutation map")
                .entry((
                    kind,
                    provider.as_str().to_owned(),
                    account.as_str().to_owned(),
                ))
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Reads one slot. Missing Credential Manager entries alone map to `None`.
    pub async fn get(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<Option<Credential>, SecretError> {
        let target = protocol::target(kind.schema(), provider.as_str(), account.as_str());
        let backend = SystemBackend::new(self.root.clone());
        let bytes = tokio::task::spawn_blocking(move || protocol::get(&backend, &target))
            .await
            .map_err(join_error)?
            .map_err(|error| match error {
                ReadError::NotUtf8 => SecretError::NotUtf8,
                ReadError::Unavailable(error) => SecretError::Unavailable(error),
            })?;
        bytes
            .map(|bytes| String::from_utf8(bytes).map(Credential::new))
            .transpose()
            .map_err(|_| SecretError::NotUtf8)
    }

    /// Stores one slot, using DPAPI when its UTF-8 byte length exceeds 2560.
    pub async fn set(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
        secret: &Credential,
    ) -> Result<(), SecretError> {
        let mutation = self.mutation(kind, provider, account);
        let _guard = mutation.lock().await;
        self.set_uncoordinated(kind, provider, account, secret)
            .await
    }

    async fn set_uncoordinated(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
        secret: &Credential,
    ) -> Result<(), SecretError> {
        let target = protocol::target(kind.schema(), provider.as_str(), account.as_str());
        let account = account.as_str().to_owned();
        let bytes = secret.expose().as_bytes().to_vec();
        let backend = SystemBackend::new(self.root.clone());
        tokio::task::spawn_blocking(move || protocol::set(&backend, &target, &account, &bytes))
            .await
            .map_err(join_error)?
            .map_err(SecretError::Unavailable)
    }

    /// Replaces a slot only if its complete current UTF-8 document matches `expected`.
    pub async fn compare_and_set(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
        expected: &Credential,
        replacement: &Credential,
    ) -> Result<bool, SecretError> {
        let mutation = self.mutation(kind, provider, account);
        let _guard = mutation.lock().await;
        let current = self.get(kind, provider, account).await?;
        if current.as_ref().map(Credential::expose) != Some(expected.expose()) {
            return Ok(false);
        }
        self.set_uncoordinated(kind, provider, account, replacement)
            .await?;
        Ok(true)
    }

    /// Deletes the marker first, then removes every generation for that slot.
    pub async fn delete(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<(), SecretError> {
        let mutation = self.mutation(kind, provider, account);
        let _guard = mutation.lock().await;
        let target = protocol::target(kind.schema(), provider.as_str(), account.as_str());
        let backend = SystemBackend::new(self.root.clone());
        tokio::task::spawn_blocking(move || protocol::delete(&backend, &target))
            .await
            .map_err(join_error)?
            .map_err(SecretError::Unavailable)
    }
}

fn join_error(error: tokio::task::JoinError) -> SecretError {
    SecretError::Unavailable(format!("Windows credential worker failed: {error}"))
}

fn fallback_root() -> Result<PathBuf, SecretError> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        SecretError::Unavailable("LOCALAPPDATA is not set for the current user".to_owned())
    })?;
    Ok(PathBuf::from(local)
        .join("tidemark")
        .join("secrets")
        .join("v1"))
}

fn secure_directory(path: &Path) -> Result<(), SecretError> {
    fs::create_dir_all(path).map_err(io_error)?;
    let sid_output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(io_error)?;
    if !sid_output.status.success() {
        return Err(SecretError::Unavailable(
            "whoami could not identify the current user SID".into(),
        ));
    }
    let row = String::from_utf8(sid_output.stdout)
        .map_err(|_| SecretError::Unavailable("whoami returned non-UTF-8 output".into()))?;
    let sid = row
        .trim()
        .trim_matches('"')
        .rsplit_once("\",\"")
        .map(|(_, sid)| sid.trim_matches('"'))
        .filter(|sid| sid.starts_with("S-1-"))
        .ok_or_else(|| SecretError::Unavailable("whoami returned a malformed SID".into()))?;
    let user_grant = format!("*{sid}:(OI)(CI)F");
    let status = std::process::Command::new("icacls.exe")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &user_grant,
            "*S-1-5-18:(OI)(CI)F",
        ])
        .status()
        .map_err(io_error)?;
    if !status.success() {
        return Err(SecretError::Unavailable(
            "could not apply the fallback directory DACL".into(),
        ));
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> SecretError {
    SecretError::Unavailable(format!("Windows secret fallback I/O failed: {error}"))
}

#[derive(Debug)]
struct SystemBackend {
    root: PathBuf,
}

impl SystemBackend {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[derive(Debug)]
struct CredAllocation(NonNull<CREDENTIALW>);

impl Drop for CredAllocation {
    fn drop(&mut self) {
        // SAFETY: FFI boundary UB/double-free: `self.0` is the unique pointer returned by
        // successful `CredReadW`; this Drop runs exactly once and CredFree accepts it.
        unsafe { CredFree(self.0.as_ptr().cast::<c_void>()) };
    }
}

#[derive(Debug)]
struct LocalAllocation(NonNull<u8>);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: FFI boundary UB/double-free: `self.0` is the unique LocalAlloc pointer
        // returned in a successful DPAPI output blob and this guard owns its sole free.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.as_ptr().cast::<c_void>()))) };
    }
}

impl Backend for SystemBackend {
    type Temp = PathBuf;

    fn read_record(&self, target: &str) -> Result<Option<Record>, String> {
        let target = wide(target);
        let mut raw = std::ptr::null_mut();
        // SAFETY: FFI boundary UB/uninitialized output: `target` is NUL-terminated and
        // alive for the call; `raw` is an initialized writable out-pointer validated below.
        let result =
            unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut raw) };
        if let Err(error) = result {
            return if is_not_found(&error) {
                Ok(None)
            } else {
                Err(win_error("CredReadW", error))
            };
        }
        let allocation = CredAllocation(
            NonNull::new(raw)
                .ok_or_else(|| "CredReadW succeeded with a null pointer".to_owned())?,
        );
        // SAFETY: FFI boundary UB/alignment/initialization: successful CredReadW returned a
        // non-null pointer to an initialized, aligned CREDENTIALW owned by `allocation`.
        let credential = unsafe { allocation.0.as_ref() };
        let blob = copy_bytes(
            credential.CredentialBlob,
            credential.CredentialBlobSize,
            "credential blob",
        )?;
        let attributes = read_attributes(credential)?;
        Ok(Some(Record { blob, attributes }))
    }

    fn write_record(&self, target: &str, account: &str, record: &Record) -> Result<(), String> {
        let mut target = wide(target);
        let mut account = wide(account);
        let mut blob = record.blob.clone();
        let mut keyword = wide(STORAGE_ATTRIBUTE);
        let mut value = STORAGE_VALUE.to_vec();
        let mut attribute = CREDENTIAL_ATTRIBUTEW {
            Keyword: PWSTR(keyword.as_mut_ptr()),
            Flags: 0,
            ValueSize: value
                .len()
                .try_into()
                .map_err(|_| "attribute is too large")?,
            Value: value.as_mut_ptr(),
        };
        let (count, attributes) = match record.attributes {
            Attributes::None => (0, std::ptr::null_mut()),
            Attributes::DpapiFileV1 => (1, std::ptr::from_mut(&mut attribute)),
            Attributes::Unknown => return Err("refusing to write unknown attributes".into()),
        };
        let credential = CREDENTIALW {
            Flags: Default::default(),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            Comment: PWSTR::null(),
            LastWritten: Default::default(),
            CredentialBlobSize: blob
                .len()
                .try_into()
                .map_err(|_| "credential blob is too large")?,
            CredentialBlob: blob.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: count,
            Attributes: attributes,
            TargetAlias: PWSTR::null(),
            UserName: PWSTR(account.as_mut_ptr()),
        };
        // SAFETY: FFI boundary UB/dangling pointers: every pointer in `credential` targets
        // initialized mutable storage retained for the call; lengths match those buffers.
        unsafe { CredWriteW(&credential, 0) }.map_err(|error| win_error("CredWriteW", error))
    }

    fn delete_record(&self, target: &str) -> Result<(), String> {
        let target = wide(target);
        // SAFETY: FFI boundary UB/string validity: `target` is NUL-terminated UTF-16 and
        // remains alive throughout CredDeleteW; no pointer escapes the call.
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if is_not_found(&error) => Ok(()),
            Err(error) => Err(win_error("CredDeleteW", error)),
        }
    }

    fn protect(&self, plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
        crypt(plaintext, entropy, true)
    }

    fn unprotect(&self, ciphertext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
        crypt(ciphertext, entropy, false)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>, String> {
        fs::read(self.root.join(name)).map_err(|error| error.to_string())
    }

    fn write_temp(&self, bytes: &[u8]) -> Result<Self::Temp, String> {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = self.root.join(format!(".tmp-{}-{id}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| error.to_string())?;
        let result = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string());
        drop(file);
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(path)
    }

    fn publish(&self, temp: Self::Temp, name: &str) -> Result<bool, String> {
        let destination = self.root.join(name);
        if destination.exists() {
            let existing = fs::read(&destination).map_err(|error| error.to_string())?;
            let staged = fs::read(&temp).map_err(|error| error.to_string())?;
            fs::remove_file(temp).map_err(|error| error.to_string())?;
            if existing != staged {
                return Err("existing immutable generation failed content validation".into());
            }
            return Ok(false);
        }
        let from = wide_os(temp.as_os_str());
        let to = wide_os(destination.as_os_str());
        // SAFETY: FFI boundary UB/string validity: both paths are NUL-terminated UTF-16
        // buffers alive for the call; MoveFileExW retains neither pointer.
        let result = unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                let staged = fs::read(&temp);
                let existing = fs::read(&destination);
                let _ = fs::remove_file(temp);
                if matches!((staged, existing), (Ok(staged), Ok(existing)) if staged == existing) {
                    Ok(false)
                } else {
                    Err(win_error("MoveFileExW", error))
                }
            }
        }
    }

    fn remove_file(&self, name: &str) -> Result<(), String> {
        match fs::remove_file(self.root.join(name)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn cleanup_generations(&self, target_hash: &str, keep: Option<&str>) -> Result<(), String> {
        for entry in fs::read_dir(&self.root).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(target_hash)
                && name.ends_with(".bin")
                && Some(name.as_ref()) != keep
            {
                fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

fn crypt(input: &[u8], entropy: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    let input_len = u32::try_from(input.len()).map_err(|_| "DPAPI input is too large")?;
    let entropy_len = u32::try_from(entropy.len()).map_err(|_| "DPAPI entropy is too large")?;
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input_len,
        pbData: input.as_ptr().cast_mut(),
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy_len,
        pbData: entropy.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    if protect {
        // SAFETY: FFI boundary UB/bounds: both input blobs point to initialized slices whose
        // checked u32 lengths match; output is writable and validated before dereference.
        unsafe {
            CryptProtectData(
                &input_blob,
                PCWSTR::null(),
                Some(&entropy_blob),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| win_error("CryptProtectData", error))?;
    } else {
        // SAFETY: FFI boundary UB/bounds: ciphertext and entropy pointers cover their
        // checked lengths; output is initialized writable storage and is validated below.
        unsafe {
            CryptUnprotectData(
                &input_blob,
                None,
                Some(&entropy_blob),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
        .map_err(|error| win_error("CryptUnprotectData", error))?;
    }
    let allocation = LocalAllocation(
        NonNull::new(output.pbData)
            .ok_or_else(|| "DPAPI succeeded with a null output".to_owned())?,
    );
    copy_bytes(allocation.0.as_ptr(), output.cbData, "DPAPI output")
}

fn read_attributes(credential: &CREDENTIALW) -> Result<Attributes, String> {
    match credential.AttributeCount {
        0 => Ok(Attributes::None),
        1 => {
            let pointer = NonNull::new(credential.Attributes)
                .ok_or_else(|| "credential attribute pointer is null".to_owned())?;
            // SAFETY: FFI boundary UB/alignment/initialization: CredReadW reports one
            // attribute and supplied a non-null pointer owned by the live credential.
            let attribute = unsafe { pointer.as_ref() };
            if attribute.Keyword.is_null() {
                return Ok(Attributes::Unknown);
            }
            // SAFETY: FFI boundary UB/string validity: CredReadW guarantees Keyword is a
            // NUL-terminated string within its allocation, which remains live here.
            let keyword = unsafe { attribute.Keyword.to_string() }
                .map_err(|_| "credential attribute keyword is invalid UTF-16".to_owned())?;
            let value = copy_bytes(attribute.Value, attribute.ValueSize, "credential attribute")?;
            if keyword == STORAGE_ATTRIBUTE && value == STORAGE_VALUE {
                Ok(Attributes::DpapiFileV1)
            } else {
                Ok(Attributes::Unknown)
            }
        }
        _ => Ok(Attributes::Unknown),
    }
}

fn copy_bytes(pointer: *mut u8, length: u32, what: &str) -> Result<Vec<u8>, String> {
    let length = usize::try_from(length).map_err(|_| format!("{what} length is invalid"))?;
    if length == 0 {
        return Ok(Vec::new());
    }
    let pointer = NonNull::new(pointer).ok_or_else(|| format!("{what} pointer is null"))?;
    // SAFETY: FFI boundary UB/out-of-bounds: the Win32 result pairs this non-null pointer
    // with `length` initialized bytes; the owning allocation remains live during the copy.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.as_ptr(), length) };
    Ok(bytes.to_vec())
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_os(value: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn is_not_found(error: &WindowsError) -> bool {
    error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0)
}

fn win_error(operation: &str, error: WindowsError) -> String {
    format!("{operation} failed: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test below uses `Store::connect`, which is rooted at the real shared
    /// `%LOCALAPPDATA%\tidemark\secrets\v1` directory. Concurrent tests performing
    /// read-modify-write cycles (or concurrent `secure_directory` DACL setup) race on
    /// that shared state, so each test holds this lock for its entire body.
    /// Any future test touching the real store MUST acquire `STORE_LOCK` first.
    static STORE_LOCK: SyncMutex<()> = SyncMutex::new(());

    fn ids(suffix: &str) -> (ProviderId, AccountId) {
        (
            ProviderId::new(format!("tidemark-test-{suffix}")),
            AccountId::default(),
        )
    }

    #[tokio::test]
    async fn boundaries_and_multibyte_utf8_round_trip_through_real_windows_storage() {
        let _guard = STORE_LOCK.lock().unwrap();
        let store = Store::connect().await.unwrap();
        for (index, value) in [
            String::new(),
            "x".into(),
            "x".repeat(2559),
            "x".repeat(2560),
            "x".repeat(2561),
            "é".repeat(1281),
        ]
        .into_iter()
        .enumerate()
        {
            let (provider, account) = ids(&format!("boundary-{index}"));
            store
                .set(Kind::Token, &provider, &account, &Credential::new(&value))
                .await
                .unwrap();
            assert_eq!(
                store
                    .get(Kind::Token, &provider, &account)
                    .await
                    .unwrap()
                    .unwrap()
                    .expose(),
                value
            );
            store
                .delete(Kind::Token, &provider, &account)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn slots_are_isolated_and_stale_cas_never_resurrects_a_value() {
        let _guard = STORE_LOCK.lock().unwrap();
        let store = Store::connect().await.unwrap();
        let provider = ProviderId::new("tidemark-test-isolation");
        let first = AccountId::new("first");
        let second = AccountId::new("second");
        store
            .set(Kind::Key, &provider, &first, &Credential::new("key"))
            .await
            .unwrap();
        store
            .set(Kind::Token, &provider, &first, &Credential::new("token"))
            .await
            .unwrap();
        store
            .set(Kind::Key, &provider, &second, &Credential::new("other"))
            .await
            .unwrap();
        assert!(
            !store
                .compare_and_set(
                    Kind::Key,
                    &provider,
                    &first,
                    &Credential::new("stale"),
                    &Credential::new("bad")
                )
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get(Kind::Token, &provider, &first)
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "token"
        );
        assert_eq!(
            store
                .get(Kind::Key, &provider, &second)
                .await
                .unwrap()
                .unwrap()
                .expose(),
            "other"
        );
        store.delete(Kind::Key, &provider, &first).await.unwrap();
        store.delete(Kind::Token, &provider, &first).await.unwrap();
        store.delete(Kind::Key, &provider, &second).await.unwrap();
    }

    #[tokio::test]
    async fn real_fallback_integrity_faults_are_never_reported_as_absent() {
        use sha2::{Digest as _, Sha256};

        let _guard = STORE_LOCK.lock().unwrap();
        let store = Store::connect().await.unwrap();
        let provider = ProviderId::new("tidemark-test-integrity");
        let other_provider = ProviderId::new("tidemark-test-integrity-other");
        let account = AccountId::default();
        store
            .set(
                Kind::Token,
                &provider,
                &account,
                &Credential::new("x".repeat(2561)),
            )
            .await
            .unwrap();
        store
            .set(
                Kind::Token,
                &other_provider,
                &account,
                &Credential::new("y".repeat(2561)),
            )
            .await
            .unwrap();
        let target = protocol::target(Kind::Token.schema(), provider.as_str(), account.as_str());
        let other_target = protocol::target(
            Kind::Token.schema(),
            other_provider.as_str(),
            account.as_str(),
        );
        let backend = SystemBackend::new(store.root.clone());
        let record = backend.read_record(&target).unwrap().unwrap();
        let other_record = backend.read_record(&other_target).unwrap().unwrap();
        let hash = |value: &str| {
            Sha256::digest(value.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let generation = std::str::from_utf8(&record.blob).unwrap();
        let other_generation = std::str::from_utf8(&other_record.blob).unwrap();
        let path = store
            .root
            .join(format!("{}-{generation}.bin", hash(&target)));
        let other_path = store
            .root
            .join(format!("{}-{other_generation}.bin", hash(&other_target)));
        let original = fs::read(&path).unwrap();

        fs::write(&path, &original[..4]).unwrap();
        assert!(matches!(
            store.get(Kind::Token, &provider, &account).await,
            Err(SecretError::Unavailable(_))
        ));
        fs::write(&path, &original).unwrap();
        let mut flipped = original.clone();
        flipped[0] ^= 1;
        fs::write(&path, flipped).unwrap();
        assert!(matches!(
            store.get(Kind::Token, &provider, &account).await,
            Err(SecretError::Unavailable(_))
        ));
        fs::write(&path, fs::read(&other_path).unwrap()).unwrap();
        assert!(matches!(
            store.get(Kind::Token, &provider, &account).await,
            Err(SecretError::Unavailable(_))
        ));
        fs::remove_file(&path).unwrap();
        assert!(matches!(
            store.get(Kind::Token, &provider, &account).await,
            Err(SecretError::Unavailable(_))
        ));

        store
            .delete(Kind::Token, &provider, &account)
            .await
            .unwrap();
        store
            .delete(Kind::Token, &other_provider, &account)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn simultaneous_cas_writers_have_exactly_one_winner() {
        let _guard = STORE_LOCK.lock().unwrap();
        let store = Arc::new(Store::connect().await.unwrap());
        let provider = ProviderId::new("tidemark-test-cas-race");
        let account = AccountId::default();
        store
            .set(Kind::Token, &provider, &account, &Credential::new("old"))
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for replacement in ["first", "second"] {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let provider = provider.clone();
            let account = account.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                store
                    .compare_and_set(
                        Kind::Token,
                        &provider,
                        &account,
                        &Credential::new("old"),
                        &Credential::new(replacement),
                    )
                    .await
                    .unwrap()
            }));
        }
        barrier.wait().await;
        let mut winners = 0;
        for task in tasks {
            winners += usize::from(task.await.unwrap());
        }
        assert_eq!(winners, 1);
        store
            .delete(Kind::Token, &provider, &account)
            .await
            .unwrap();
    }
}
