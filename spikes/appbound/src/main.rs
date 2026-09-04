#![allow(unsafe_code)]
//! Investigation-only probe of Chrome's documented IElevatorChrome::DecryptData path.
//!
//! This deliberately reports only activation/decryption status. It never prints the
//! App-Bound blob, decrypted key material, or cookie values.

use std::ffi::c_void;
use std::path::PathBuf;

use base64::Engine as _;
use windows::Win32::Foundation::{SysAllocStringByteLen, SysStringByteLen};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::{BSTR, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

// Google Chrome Elevation Service AppID/CLSID and the vendor-specific IElevator IID from
// Chromium's elevation_service_idl.idl. The service validates that DecryptData's caller is
// the same installed browser identity that encrypted the data.
const CHROME_ELEVATOR: GUID = GUID::from_u128(0x708860e0_f641_4611_8895_7d867dd3675b);
const IID_IELEVATOR_CHROME: GUID = GUID::from_u128(0x1bf5208b_295f_4992_b5f4_3a9bb6494838);

#[derive(Clone)]
#[repr(transparent)]
struct IElevatorChrome(IUnknown);

unsafe impl Interface for IElevatorChrome {
    type Vtable = IElevatorChromeVtbl;
    const IID: GUID = IID_IELEVATOR_CHROME;
}

#[repr(C)]
struct IElevatorChromeVtbl {
    base__: IUnknown_Vtbl,
    run_recovery_crx_elevated: usize,
    encrypt_data: usize,
    decrypt_data: unsafe extern "system" fn(
        this: *mut c_void,
        ciphertext: *const u16,
        plaintext: *mut *mut u16,
        last_error: *mut u32,
    ) -> HRESULT,
}

fn main() {
    let local_state = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Google/Chrome/User Data/Local State"));
    let Some(path) = local_state.filter(|path| path.is_file()) else {
        println!("APPBOUND_GATE=NO_CHROME_LOCAL_STATE");
        return;
    };
    let result = probe(&path);
    match result {
        Ok(length) => println!("APPBOUND_GATE=DECRYPTED bytes={length}"),
        Err(error) => println!("APPBOUND_GATE=UNAVAILABLE {error}"),
    }
}

fn probe(path: &std::path::Path) -> Result<u32, String> {
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(path).map_err(|error| format!("Local State read failed: {error}"))?,
    )
    .map_err(|error| format!("Local State JSON failed: {error}"))?;
    let encoded = document
        .pointer("/os_crypt/app_bound_encrypted_key")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "app_bound_encrypted_key absent".to_owned())?;
    let wrapped = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("app-bound base64 failed: {error}"))?;
    let ciphertext = wrapped
        .strip_prefix(b"APPB")
        .ok_or_else(|| "app-bound key has no APPB prefix".to_owned())?;

    // SAFETY: COM is initialized on this thread; all pointers are supplied by windows-rs or
    // the service and remain live for each call. BSTR owners free their allocations on drop.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if initialized.is_err() {
            return Err(format!(
                "CoInitializeEx HRESULT={:#010x}",
                initialized.0 as u32
            ));
        }
        let elevator: IElevatorChrome =
            CoCreateInstance(&CHROME_ELEVATOR, None, CLSCTX_LOCAL_SERVER).map_err(|error| {
                format!("CoCreateInstance HRESULT={:#010x}", error.code().0 as u32)
            })?;
        let input = SysAllocStringByteLen(Some(ciphertext));
        let mut output = std::ptr::null_mut::<u16>();
        let mut last_error = 0u32;
        let vtable = elevator.vtable();
        let status = ((*vtable).decrypt_data)(
            Interface::as_raw(&elevator),
            input.as_ptr(),
            &mut output,
            &mut last_error,
        );
        if status.is_err() {
            return Err(format!(
                "DecryptData HRESULT={:#010x} last_error={last_error}",
                status.0 as u32
            ));
        }
        let output = BSTR::from_raw(output);
        Ok(SysStringByteLen(&output))
    }
}
