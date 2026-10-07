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

/// Where this launch stands after asking for the name.
#[derive(Debug)]
pub enum Claim {
    /// The first client: the connection holds the name for as long as it lives.
    First(zbus::Connection),
    /// Another client holds it and has been asked to come forward.
    Running,
}

pub async fn claim() -> zbus::Result<Claim> {
    let connection = zbus::Connection::session().await?;
    match connection
        .request_name_with_flags(ids::APP_ID, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {
            Ok(Claim::First(connection))
        }
        Ok(RequestNameReply::Exists | RequestNameReply::InQueue) | Err(zbus::Error::NameTaken) => {
            activate_running(&connection).await?;
            Ok(Claim::Running)
        }
        Err(error) => Err(error),
    }
}

/// Passes the launcher's activation token on, as GApplication does: a GTK owner uses it to
/// take focus on Wayland. This client cannot yet — winit has no way to activate an existing
/// window with someone else's token — and asks for focus with its own.
async fn activate_running(connection: &zbus::Connection) -> zbus::Result<()> {
    let mut platform_data: HashMap<&str, Value<'_>> = HashMap::new();
    if let Ok(token) = std::env::var("XDG_ACTIVATION_TOKEN") {
        platform_data.insert("activation-token", Value::from(token));
    }
    if let Ok(id) = std::env::var("DESKTOP_STARTUP_ID") {
        platform_data.insert("desktop-startup-id", Value::from(id));
    }
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
