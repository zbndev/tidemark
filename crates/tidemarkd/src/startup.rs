//! User-session startup integrations controlled by application preferences.

use std::path::Path;
use std::process::Command;

use tidemark_core::paths;
use tidemark_types::{Preferences, ids};

/// Applies the coherent login-start mode outside `config.toml`.
pub trait Startup: std::fmt::Debug + Send + Sync {
    fn set_startup_mode(&self, mode: &str) -> Result<(), String>;
}

#[derive(Debug, Default)]
pub struct System;

impl Startup for System {
    fn set_startup_mode(&self, mode: &str) -> Result<(), String> {
        let (desktop, daemon) =
            startup_targets(mode).ok_or_else(|| format!("unknown startup mode {mode:?}"))?;
        let config_dir = paths::config_dir().map_err(|error| error.to_string())?;
        let config_home = config_dir
            .parent()
            .ok_or_else(|| format!("{} has no parent", config_dir.display()))?;
        set_desktop_autostart_in(config_home, desktop)?;
        set_daemon_autostart(daemon)
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

fn systemctl_arguments(enabled: bool) -> [&'static str; 3] {
    [
        "--user",
        if enabled { "enable" } else { "disable" },
        "tidemarkd.service",
    ]
}

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

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("tidemark-startup-{name}-{}", std::process::id()))
    }

    #[test]
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
