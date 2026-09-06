//! User-session startup integrations controlled by application preferences.

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

#[cfg(unix)]
use tidemark_core::paths;
use tidemark_types::Preferences;
#[cfg(unix)]
use tidemark_types::ids;

/// Applies the coherent login-start mode outside `config.toml`.
pub trait Startup: std::fmt::Debug + Send + Sync {
    fn set_startup_mode(&self, mode: &str) -> Result<(), String>;
}

/// The Windows arm of the mode map: the daemon's login start is the per-user
/// Scheduled Task, the UI's is the HKCU Run value, and "off" removes both. The
/// next to the singleton mutex and the job primitive it shares the lifecycle surface
/// with.

#[derive(Debug, Default)]
pub struct System;

impl Startup for System {
    fn set_startup_mode(&self, mode: &str) -> Result<(), String> {
        let (desktop, daemon) =
            startup_targets(mode).ok_or_else(|| format!("unknown startup mode {mode:?}"))?;
        #[cfg(windows)]
        {
            crate::lifecycle::set_ui_run(desktop)?;
            crate::lifecycle::set_daemon_task(daemon)
        }
        #[cfg(unix)]
        {
            let config_dir = paths::config_dir().map_err(|error| error.to_string())?;
            let config_home = config_dir
                .parent()
                .ok_or_else(|| format!("{} has no parent", config_dir.display()))?;
            set_desktop_autostart_in(config_home, desktop)?;
            set_daemon_autostart(daemon)
        }
    }
}

fn startup_targets(mode: &str) -> Option<(bool, bool)> {
    match mode {
        Preferences::STARTUP_APP => Some((true, false)),
        Preferences::STARTUP_DAEMON => Some((false, true)),
        Preferences::STARTUP_OFF => Some((false, false)),
        _ => None,
    }
}

#[cfg(unix)]
fn set_daemon_autostart(enabled: bool) -> Result<(), String> {
    let output = Command::new("systemctl")
        .args(systemctl_arguments(enabled))
        .output()
        .map_err(|error| format!("could not run systemctl: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "systemctl could not {} tidemarkd.service: {}",
        if enabled { "enable" } else { "disable" },
        stderr.trim()
    ))
}

#[cfg(unix)]
fn systemctl_arguments(enabled: bool) -> [&'static str; 3] {
    [
        "--user",
        if enabled { "enable" } else { "disable" },
        "tidemarkd.service",
    ]
}

#[cfg(unix)]
fn set_desktop_autostart_in(config_home: &Path, enabled: bool) -> Result<(), String> {
    let directory = config_home.join("autostart");
    let path = directory.join(format!("{}.desktop", ids::APP_ID));
    if enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove {}: {error}", path.display())),
        };
    }

    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let temporary = path.with_extension(format!("desktop.tmp-{}", std::process::id()));
    std::fs::write(&temporary, "[Desktop Entry]\nHidden=true\n")
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("could not replace {}: {error}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tidemark-startup-{name}-{}", std::process::id()))
    }

    /// The Linux-arm tests are the plan's byte-identical guard: they pin the XDG
    /// override and the systemd unit, so they only exist where those paths exist.
    #[test]
    #[cfg(unix)]
    fn disabling_desktop_autostart_writes_the_standard_xdg_override() {
        let directory = scratch("desktop-off");
        let _ = std::fs::remove_dir_all(&directory);

        set_desktop_autostart_in(&directory, false).expect("disabled");

        let override_path = directory
            .join("autostart")
            .join("io.github.zbndev.Tidemark.desktop");
        assert_eq!(
            std::fs::read_to_string(override_path).expect("override exists"),
            "[Desktop Entry]\nHidden=true\n"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg(unix)]
    fn enabling_desktop_autostart_removes_the_user_override() {
        let directory = scratch("desktop-on");
        let override_path = directory
            .join("autostart")
            .join("io.github.zbndev.Tidemark.desktop");
        std::fs::create_dir_all(override_path.parent().expect("has parent")).expect("directory");
        std::fs::write(&override_path, "[Desktop Entry]\nHidden=true\n").expect("seeded");

        set_desktop_autostart_in(&directory, true).expect("enabled");

        assert!(!override_path.exists());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    #[cfg(unix)]
    fn daemon_autostart_uses_the_user_systemd_unit() {
        assert_eq!(
            systemctl_arguments(true),
            ["--user", "enable", "tidemarkd.service"]
        );
        assert_eq!(
            systemctl_arguments(false),
            ["--user", "disable", "tidemarkd.service"]
        );
    }
    #[test]
    fn three_startup_modes_have_unambiguous_targets() {
        assert_eq!(startup_targets("app"), Some((true, false)));
        assert_eq!(startup_targets("daemon"), Some((false, true)));
        assert_eq!(startup_targets("off"), Some((false, false)));
        assert_eq!(startup_targets("everything"), None);
    }
}
