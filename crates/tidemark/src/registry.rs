#![allow(unsafe_code)]
//! The desktop's appearance on Windows: the apps light/dark choice and the accent colour,
//! read from the user's registry — what the XDG portal is to `portal` on Linux.
//!
//! Windows announces a change with `WM_SETTINGCHANGE` to top-level windows, and the window
//! here is winit's, so the message is not ours to receive. Two `DWORD`s read every couple
//! of seconds is the cheaper honest answer.
//!
//! This module is a locally-audited `unsafe` island like `single_instance`: the generated
//! registry bindings are unsafe functions, and nothing raw escapes them.

use std::time::Duration;

use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::{PCWSTR, w};

use crate::window::Appearance;

const POLL: Duration = Duration::from_secs(2);

/// Reports the current appearance, then every change to it.
pub fn watch(on: impl Fn(Appearance) + 'static) {
    let spawned = slint::spawn_local(async move {
        let mut dark = None;
        let mut accent = None;
        loop {
            let now_dark = apps_use_dark();
            if now_dark != dark {
                dark = now_dark;
                on(Appearance::Dark(now_dark.unwrap_or(false)));
            }
            let now_accent = accent_color();
            if now_accent != accent {
                accent = now_accent;
                on(Appearance::Accent(now_accent.map(rgb)));
            }
            async_io::Timer::after(POLL).await;
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "the event loop refused the appearance watcher");
    }
}

/// `AppsUseLightTheme` is 0 when the user chose dark for apps. Absent on systems that
/// predate the setting, which means light.
fn apps_use_dark() -> Option<bool> {
    read_dword(
        w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        w!("AppsUseLightTheme"),
    )
    .map(|light| light == 0)
}

/// The accent as DWM stores it, `0xAABBGGRR`.
fn accent_color() -> Option<u32> {
    read_dword(w!(r"Software\Microsoft\Windows\DWM"), w!("AccentColor"))
}

fn rgb(abgr: u32) -> [f64; 3] {
    let channel = |shift: u32| f64::from((abgr >> shift) & 0xff) / 255.0;
    [channel(0), channel(8), channel(16)]
}

fn read_dword(key: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: `key` and `value` are static nul-terminated strings from `w!`; `data` and
    // `size` are live locals of the size the call is told about, and only a DWORD is
    // accepted, so nothing larger can be written.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&raw mut size),
        )
    };
    status.is_ok().then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accent_is_read_red_first_from_its_low_byte() {
        assert_eq!(
            rgb(0xff_33_66_99),
            [
                0x99 as f64 / 255.0,
                0x66 as f64 / 255.0,
                0x33 as f64 / 255.0
            ]
        );
    }
}
