//! One desktop client per session (Linux), the way GApplication made it one: the first
//! client owns the application ID on the session bus and serves
//! `org.freedesktop.Application` at the ID's path; a second launch finds the name taken,
//! asks the owner to activate, and exits.
//!
//! The interface is the freedesktop one, not a private method, so a running GTK client of
//! an older version is raised by a new launch too, and the other way round.

use std::collections::HashMap;

use tidemark_types::ids;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::zvariant::{OwnedValue, Value};

use crate::AppWindow;

const INTERFACE: &str = "org.freedesktop.Application";

/// The launcher metadata, retained until single-instance forwarding or window creation.
#[derive(Debug, Default)]
pub struct Launch {
    wayland: Option<String>,
    x11: Option<String>,
}

impl Launch {
    /// Call at the start of main, before tracing, D-Bus or Slint can create threads.
    /// Clearing the environment here keeps the one-use tokens out of child processes;
    /// the captured copy still reaches an existing application through D-Bus.
    pub fn from_env_at_startup() -> Self {
        let launch = Self {
            wayland: std::env::var("XDG_ACTIVATION_TOKEN").ok(),
            x11: std::env::var("DESKTOP_STARTUP_ID").ok(),
        };
        #[cfg(target_os = "linux")]
        slint::winit_030::winit::platform::startup_notify::reset_activation_token_env();
        launch
    }

    fn platform_data(&self) -> HashMap<&str, Value<'static>> {
        let mut data = HashMap::new();
        if let Some(token) = &self.wayland {
            data.insert("activation-token", Value::from(token.clone()));
        }
        if let Some(id) = &self.x11 {
            data.insert("desktop-startup-id", Value::from(id.clone()));
        }
        data
    }

    #[cfg(target_os = "linux")]
    pub fn window_attributes(
        &mut self,
        attributes: slint::winit_030::winit::window::WindowAttributes,
    ) -> slint::winit_030::winit::window::WindowAttributes {
        use slint::winit_030::winit::platform::startup_notify::WindowAttributesExtStartupNotify;
        use slint::winit_030::winit::window::ActivationToken;

        // Prefer the Wayland launch token, with the startup ID as the X11 fallback.
        // Both are taken so a later window cannot replay the other token.
        let wayland = self.wayland.take().filter(|token| !token.is_empty());
        let x11 = self.x11.take().filter(|id| !id.is_empty());
        match wayland.or(x11) {
            Some(token) => attributes.with_activation_token(ActivationToken::from_raw(token)),
            None => attributes,
        }
    }
}

/// Where this launch stands after asking for the name.
#[derive(Debug)]
pub enum Claim {
    /// The first client: the connection holds the name for as long as it lives.
    First(zbus::Connection),
    /// Another client holds it, and has been asked to come forward unless this is a
    /// background start.
    Running,
}

/// `activate` is false for the session's autostart, which finding a client already running
/// leaves alone rather than raising its window at login.
pub async fn claim(activate: bool, launch: &Launch) -> zbus::Result<Claim> {
    let connection = zbus::Connection::session().await?;
    match connection
        .request_name_with_flags(ids::APP_ID, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Claim::First(connection))
        }
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            if activate {
                activate_running(&connection, launch).await?;
            }
            Ok(Claim::Running)
        }
        Err(error) => Err(error),
    }
}

/// Passes the launcher's activation token on, as GApplication does: a GTK owner uses it to
/// take focus on Wayland. This client cannot yet — winit has no way to activate an existing
/// window with someone else's token — and asks for focus with its own.
async fn activate_running(connection: &zbus::Connection, launch: &Launch) -> zbus::Result<()> {
    let platform_data = launch.platform_data();
    connection
        .call_method(
            Some(ids::APP_ID),
            ids::OBJECT_PATH,
            Some(INTERFACE),
            "Activate",
            &(platform_data,),
        )
        .await?;
    Ok(())
}

/// Serves activation for the window once it exists. A launch that arrives between the
/// name and this is told the object is missing, and its window is not raised: the window
/// is about to appear anyway.
pub async fn serve(connection: &zbus::Connection, ui: &AppWindow) -> zbus::Result<()> {
    connection
        .object_server()
        .at(
            ids::OBJECT_PATH,
            Application {
                ui: slint::ComponentHandle::as_weak(ui),
            },
        )
        .await?;
    Ok(())
}

struct Application {
    ui: slint::Weak<AppWindow>,
}

#[zbus::interface(name = "org.freedesktop.Application")]
impl Application {
    // Called on zbus's own thread; the window is the event loop's.
    fn activate(&self, _platform_data: HashMap<String, OwnedValue>) {
        if let Err(error) = self
            .ui
            .upgrade_in_event_loop(|ui| crate::window::present(&ui))
        {
            tracing::warn!(%error, "could not reach the window to bring it forward");
        }
    }

    fn open(&self, _uris: Vec<String>, platform_data: HashMap<String, OwnedValue>) {
        self.activate(platform_data);
    }

    fn activate_action(
        &self,
        _name: String,
        _parameter: Vec<OwnedValue>,
        platform_data: HashMap<String, OwnedValue>,
    ) {
        self.activate(platform_data);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::Launch;
    use slint::winit_030::winit::window::WindowAttributes;

    #[test]
    fn the_first_window_receives_the_launch_token_and_later_windows_do_not() {
        let mut launch = Launch {
            wayland: Some("wayland-launch-token".into()),
            x11: Some("legacy-launch-id".into()),
        };
        // Winit exposes the builder's platform fields through Debug, but no getter.
        let first = format!(
            "{:?}",
            launch.window_attributes(WindowAttributes::default())
        );
        assert!(
            first.contains("wayland-launch-token"),
            "missing activation token: {first}"
        );
        let later = format!(
            "{:?}",
            launch.window_attributes(WindowAttributes::default())
        );
        assert!(!later.contains("wayland-launch-token") && !later.contains("legacy-launch-id"));
    }

    #[test]
    fn an_x11_launch_supplies_its_startup_id_to_the_window() {
        let mut launch = Launch {
            wayland: None,
            x11: Some("legacy-launch-id".into()),
        };
        let attributes = format!(
            "{:?}",
            launch.window_attributes(WindowAttributes::default())
        );
        assert!(
            attributes.contains("legacy-launch-id"),
            "missing startup ID: {attributes}"
        );
    }

    #[test]
    fn a_second_launch_keeps_both_tokens_for_the_existing_application() {
        let launch = Launch {
            wayland: Some("wayland-launch-token".into()),
            x11: Some("legacy-launch-id".into()),
        };
        let data = launch.platform_data();
        assert_eq!(
            data["activation-token"].downcast_ref::<&str>().unwrap(),
            "wayland-launch-token"
        );
        assert_eq!(
            data["desktop-startup-id"].downcast_ref::<&str>().unwrap(),
            "legacy-launch-id"
        );
    }
}
