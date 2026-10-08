#![allow(unsafe_code)]
//! Native setup coordination. A maintenance request exits the event loop, including
//! a client hidden in the tray; the installer waits for the daemon before sending it.

use std::io;
use windows::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::HSTRING;

pub fn active() -> io::Result<bool> {
    // SAFETY: named read-only open; the handle is closed before returning.
    match unsafe {
        OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            &HSTRING::from(tidemark_types::ids::INSTALLER_MUTEX),
        )
    } {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            Ok(true)
        }
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(false),
        Err(error) => Err(io::Error::other(error)),
    }
}

pub fn watch_stop() -> io::Result<()> {
    // SAFETY: non-inherited, named manual-reset event; ownership moves to the thread.
    let event = unsafe {
        CreateEventW(
            None,
            true,
            false,
            &HSTRING::from(format!(
                "{}.{}",
                tidemark_types::ids::CLIENT_STOP_EVENT,
                std::process::id()
            )),
        )
    }
    .map_err(io::Error::other)?;
    let raw = event.0 as usize;
    if let Err(error) = std::thread::Builder::new()
        .name("installer-stop".into())
        .spawn(move || {
            let handle = HANDLE(raw as _);
            // SAFETY: this thread exclusively owns the event handle until after the wait.
            let stopped = unsafe { WaitForSingleObject(handle, INFINITE) } == WAIT_OBJECT_0;
            let _ = unsafe { CloseHandle(handle) };
            if stopped {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            } else {
                tracing::error!("could not wait for installer shutdown request");
            }
        })
    {
        // SAFETY: failed spawn did not transfer ownership to a thread.
        let _ = unsafe { CloseHandle(event) };
        return Err(error);
    }
    Ok(())
}
