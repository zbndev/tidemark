//! Platform-neutral Credential Manager/DPAPI fallback protocol.
//!
//! Win32 calls and raw pointers stay in `windows_store`; this module owns the format,
//! integrity checks, and the marker-as-linearization-point state machine.

use sha2::{Digest, Sha256};

pub(super) const INLINE_LIMIT: usize = 2560;
pub(super) const HEADER: &[u8] = b"TIDEMARK-DPAPI\0";
pub(super) const VERSION: u8 = 1;
#[cfg(windows)]
pub(super) const STORAGE_ATTRIBUTE: &str = "Tidemark_Storage";
#[cfg(windows)]
pub(super) const STORAGE_VALUE: &[u8] = b"dpapi-file-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Attributes {
    None,
    DpapiFileV1,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Record {
    pub(super) blob: Vec<u8>,
    pub(super) attributes: Attributes,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReadError {
    NotUtf8,
    Unavailable(String),
}

impl From<String> for ReadError {
    fn from(error: String) -> Self {
        Self::Unavailable(error)
    }
}

pub(super) trait Backend {
    type Temp;

    fn read_record(&self, target: &str) -> Result<Option<Record>, String>;
    fn write_record(&self, target: &str, account: &str, record: &Record) -> Result<(), String>;
    fn delete_record(&self, target: &str) -> Result<(), String>;
    fn protect(&self, plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String>;
    fn unprotect(&self, ciphertext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String>;
    fn read_file(&self, name: &str) -> Result<Vec<u8>, String>;
    fn write_temp(&self, bytes: &[u8]) -> Result<Self::Temp, String>;
    /// Publishes a temp file and returns whether this call created the immutable generation.
    fn publish(&self, temp: Self::Temp, name: &str) -> Result<bool, String>;
    fn remove_file(&self, name: &str) -> Result<(), String>;
    fn cleanup_generations(&self, target_hash: &str, keep: Option<&str>) -> Result<(), String>;
}

#[cfg(windows)]
pub(super) fn target(schema: &str, provider: &str, account: &str) -> String {
    format!("{schema}/{provider}/{account}")
}

pub(super) fn get<B: Backend>(backend: &B, target: &str) -> Result<Option<Vec<u8>>, ReadError> {
    let Some(record) = backend.read_record(target)? else {
        return Ok(None);
    };
    match record.attributes {
        Attributes::None => String::from_utf8(record.blob)
            .map(String::into_bytes)
            .map(Some)
            .map_err(|_| ReadError::NotUtf8),
        Attributes::Unknown => Err(ReadError::Unavailable(
            "credential has unknown or malformed storage attributes".to_owned(),
        )),
        Attributes::DpapiFileV1 => {
            let generation = parse_generation(&record.blob)?;
            let target_hash = digest_hex(target.as_bytes());
            let name = format!("{target_hash}-{generation}.bin");
            let file = backend
                .read_file(&name)
                .map_err(|error| format!("DPAPI fallback is unavailable: {error}"))?;
            if digest_hex(&file) != generation {
                return Err(ReadError::Unavailable(
                    "DPAPI fallback integrity check failed".to_owned(),
                ));
            }
            let prefix_len = HEADER.len() + 1;
            if file.len() <= prefix_len || !file.starts_with(HEADER) {
                return Err(ReadError::Unavailable(
                    "DPAPI fallback header is malformed".to_owned(),
                ));
            }
            if file[HEADER.len()] != VERSION {
                return Err(ReadError::Unavailable(
                    "DPAPI fallback version is unknown".to_owned(),
                ));
            }
            let plaintext = backend.unprotect(&file[prefix_len..], target.as_bytes())?;
            String::from_utf8(plaintext)
                .map(String::into_bytes)
                .map(Some)
                .map_err(|_| {
                    ReadError::Unavailable("DPAPI fallback plaintext is not valid UTF-8".to_owned())
                })
        }
    }
}

pub(super) fn set<B: Backend>(
    backend: &B,
    target: &str,
    account: &str,
    secret: &[u8],
) -> Result<(), String> {
    let target_hash = digest_hex(target.as_bytes());
    if secret.len() <= INLINE_LIMIT {
        backend.write_record(
            target,
            account,
            &Record {
                blob: secret.to_vec(),
                attributes: Attributes::None,
            },
        )?;
        return backend.cleanup_generations(&target_hash, None);
    }

    let protected = backend.protect(secret, target.as_bytes())?;
    let mut file = Vec::with_capacity(HEADER.len() + 1 + protected.len());
    file.extend_from_slice(HEADER);
    file.push(VERSION);
    file.extend_from_slice(&protected);
    let generation = digest_hex(&file);
    let name = format!("{target_hash}-{generation}.bin");
    let temp = backend.write_temp(&file)?;
    let published = backend.publish(temp, &name)?;
    let marker = Record {
        blob: generation.as_bytes().to_vec(),
        attributes: Attributes::DpapiFileV1,
    };
    if let Err(error) = backend.write_record(target, account, &marker) {
        if published {
            let _ = backend.remove_file(&name);
        }
        return Err(error);
    }
    backend.cleanup_generations(&target_hash, Some(&name))
}

pub(super) fn delete<B: Backend>(backend: &B, target: &str) -> Result<(), String> {
    backend.delete_record(target)?;
    backend.cleanup_generations(&digest_hex(target.as_bytes()), None)
}

fn parse_generation(blob: &[u8]) -> Result<String, String> {
    let generation =
        std::str::from_utf8(blob).map_err(|_| "DPAPI fallback marker is not UTF-8".to_owned())?;
    if generation.len() != 64
        || !generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("DPAPI fallback marker is malformed".to_owned());
    }
    Ok(generation.to_owned())
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing into a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Cut {
        Temp,
        Publish,
        Marker,
        MarkerFailure,
        Cleanup,
    }

    #[derive(Debug, Default)]
    struct State {
        records: HashMap<String, Record>,
        files: HashMap<String, Vec<u8>>,
        temps: HashMap<usize, Vec<u8>>,
        next_temp: usize,
        cut: Option<Cut>,
    }

    #[derive(Debug, Default)]
    struct Fake(Mutex<State>);

    impl Fake {
        fn cut(&self, cut: Cut) {
            self.0.lock().unwrap().cut = Some(cut);
        }

        fn damage(&self, name: &str, change: impl FnOnce(&mut HashMap<String, Vec<u8>>)) {
            let mut state = self.0.lock().unwrap();
            assert!(state.files.contains_key(name));
            change(&mut state.files);
        }

        fn marker_name(&self, target: &str) -> String {
            let state = self.0.lock().unwrap();
            let generation = std::str::from_utf8(&state.records[target].blob).unwrap();
            format!("{}-{generation}.bin", digest_hex(target.as_bytes()))
        }
    }

    impl Backend for Fake {
        type Temp = usize;

        fn read_record(&self, target: &str) -> Result<Option<Record>, String> {
            Ok(self.0.lock().unwrap().records.get(target).cloned())
        }

        fn write_record(&self, target: &str, _: &str, record: &Record) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            match state.cut.take() {
                Some(Cut::MarkerFailure) => return Err("marker failure".into()),
                Some(Cut::Marker) => {
                    state.records.insert(target.to_owned(), record.clone());
                    drop(state);
                    panic!("simulated crash after marker write");
                }
                _ => {}
            }
            state.records.insert(target.to_owned(), record.clone());
            Ok(())
        }

        fn delete_record(&self, target: &str) -> Result<(), String> {
            self.0.lock().unwrap().records.remove(target);
            Ok(())
        }

        fn protect(&self, plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
            Ok(plaintext
                .iter()
                .zip(entropy.iter().cycle())
                .map(|(left, right)| left ^ right)
                .collect())
        }

        fn unprotect(&self, ciphertext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
            self.protect(ciphertext, entropy)
        }

        fn read_file(&self, name: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .files
                .get(name)
                .cloned()
                .ok_or_else(|| "file is missing".into())
        }

        fn write_temp(&self, bytes: &[u8]) -> Result<Self::Temp, String> {
            let mut state = self.0.lock().unwrap();
            let id = state.next_temp;
            state.next_temp += 1;
            state.temps.insert(id, bytes.to_vec());
            if state.cut == Some(Cut::Temp) {
                state.cut = None;
                drop(state);
                panic!("simulated crash after temp write");
            }
            Ok(id)
        }

        fn publish(&self, temp: Self::Temp, name: &str) -> Result<bool, String> {
            let mut state = self.0.lock().unwrap();
            let bytes = state.temps.remove(&temp).unwrap();
            let published = if state.files.contains_key(name) {
                false
            } else {
                state.files.insert(name.to_owned(), bytes);
                true
            };
            if state.cut == Some(Cut::Publish) {
                state.cut = None;
                drop(state);
                panic!("simulated crash after publish");
            }
            Ok(published)
        }

        fn remove_file(&self, name: &str) -> Result<(), String> {
            self.0.lock().unwrap().files.remove(name);
            Ok(())
        }

        fn cleanup_generations(&self, prefix: &str, keep: Option<&str>) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            state
                .files
                .retain(|name, _| !name.starts_with(prefix) || Some(name.as_str()) == keep);
            if state.cut == Some(Cut::Cleanup) {
                state.cut = None;
                drop(state);
                panic!("simulated crash after cleanup");
            }
            Ok(())
        }
    }

    const TARGET: &str = "io.github.zbndev.Tidemark.ProviderToken/codex/default";

    #[test]
    fn the_inline_boundary_is_byte_based_and_preserves_utf8() {
        for size in [0, 1, 2559, 2560, 2561] {
            let backend = Fake::default();
            let value = "x".repeat(size);
            set(&backend, TARGET, "default", value.as_bytes()).unwrap();
            assert_eq!(get(&backend, TARGET).unwrap().unwrap(), value.as_bytes());
            let marker = &backend.0.lock().unwrap().records[TARGET].attributes;
            assert_eq!(*marker == Attributes::DpapiFileV1, size > INLINE_LIMIT);
        }
        let backend = Fake::default();
        let value = "é".repeat(1281);
        set(&backend, TARGET, "default", value.as_bytes()).unwrap();
        assert_eq!(get(&backend, TARGET).unwrap().unwrap(), value.as_bytes());
        assert_eq!(
            backend.0.lock().unwrap().records[TARGET].attributes,
            Attributes::DpapiFileV1
        );
    }

    #[test]
    fn every_crash_cut_reads_the_previous_or_complete_new_value() {
        for cut in [Cut::Temp, Cut::Publish, Cut::Marker, Cut::Cleanup] {
            let backend = Fake::default();
            let old = vec![b'a'; 2561];
            let new = vec![b'b'; 2562];
            set(&backend, TARGET, "default", &old).unwrap();
            backend.cut(cut);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                set(&backend, TARGET, "default", &new)
            }));
            let current = get(&backend, TARGET).unwrap().unwrap();
            assert!(
                current == old || current == new,
                "cut {cut:?} returned partial data"
            );
        }
    }

    #[test]
    fn a_marker_write_failure_removes_the_new_generation() {
        let backend = Fake::default();
        let old = vec![b'a'; 2561];
        set(&backend, TARGET, "default", &old).unwrap();
        backend.cut(Cut::MarkerFailure);
        assert!(set(&backend, TARGET, "default", &vec![b'b'; 2561]).is_err());
        assert_eq!(get(&backend, TARGET).unwrap().unwrap(), old);
        assert_eq!(backend.0.lock().unwrap().files.len(), 1);
    }

    #[test]
    fn malformed_and_damaged_fallbacks_are_unavailable_not_absent() {
        let cases: [fn(&Fake, &str); 4] = [
            |backend, name| backend.damage(name, |files| files.get_mut(name).unwrap().truncate(4)),
            |backend, name| backend.damage(name, |files| files.get_mut(name).unwrap()[0] ^= 1),
            |backend, name| {
                backend.damage(name, |files| {
                    let mut bytes = files.remove(name).unwrap();
                    bytes.push(1);
                    files.insert(name.to_owned(), bytes);
                })
            },
            |backend, name| {
                backend.damage(name, |files| {
                    files.remove(name);
                })
            },
        ];
        for damage in cases {
            let backend = Fake::default();
            set(&backend, TARGET, "default", &vec![b'x'; 2561]).unwrap();
            let name = backend.marker_name(TARGET);
            damage(&backend, &name);
            assert!(get(&backend, TARGET).is_err());
        }
    }

    #[test]
    fn swapped_fallback_files_fail_integrity_validation() {
        let backend = Fake::default();
        let other = "io.github.zbndev.Tidemark.ProviderToken/codex/other";
        set(&backend, TARGET, "default", &vec![b'x'; 2561]).unwrap();
        set(&backend, other, "other", &vec![b'y'; 2561]).unwrap();
        let first_name = backend.marker_name(TARGET);
        let second_name = backend.marker_name(other);
        let mut state = backend.0.lock().unwrap();
        let second = state.files[&second_name].clone();
        state.files.insert(first_name, second);
        drop(state);
        assert!(get(&backend, TARGET).is_err());
    }

    #[test]
    fn delete_removes_the_marker_before_generations() {
        let backend = Fake::default();
        set(&backend, TARGET, "default", &vec![b'x'; 2561]).unwrap();
        delete(&backend, TARGET).unwrap();
        assert_eq!(get(&backend, TARGET).unwrap(), None);
        assert!(backend.0.lock().unwrap().files.is_empty());
    }

    #[test]
    fn an_unknown_version_is_unavailable_after_integrity_validation() {
        let backend = Fake::default();
        set(&backend, TARGET, "default", &vec![b'x'; 2561]).unwrap();
        let old_name = backend.marker_name(TARGET);
        let mut state = backend.0.lock().unwrap();
        let mut file = state.files.remove(&old_name).unwrap();
        file[HEADER.len()] = VERSION + 1;
        let generation = digest_hex(&file);
        let name = format!("{}-{generation}.bin", digest_hex(TARGET.as_bytes()));
        state.files.insert(name, file);
        state.records.get_mut(TARGET).unwrap().blob = generation.into_bytes();
        drop(state);
        assert!(matches!(
            get(&backend, TARGET),
            Err(ReadError::Unavailable(_))
        ));
    }

    #[test]
    fn invalid_inline_utf8_remains_the_specific_not_utf8_error() {
        let backend = Fake::default();
        backend.0.lock().unwrap().records.insert(
            TARGET.into(),
            Record {
                blob: vec![0xff],
                attributes: Attributes::None,
            },
        );
        assert_eq!(get(&backend, TARGET), Err(ReadError::NotUtf8));
    }

    #[test]
    fn missing_and_unknown_markers_never_look_absent() {
        let backend = Fake::default();
        backend.0.lock().unwrap().records.insert(
            TARGET.into(),
            Record {
                blob: b"bad".to_vec(),
                attributes: Attributes::DpapiFileV1,
            },
        );
        assert!(get(&backend, TARGET).is_err());
        backend
            .0
            .lock()
            .unwrap()
            .records
            .get_mut(TARGET)
            .unwrap()
            .attributes = Attributes::Unknown;
        assert!(get(&backend, TARGET).is_err());
    }
}
