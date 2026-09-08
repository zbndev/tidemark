//! The one connection this program makes.
//!
//! Nothing here inspects `systemctl`: the daemon is D-Bus-activated, so asking for it is
//! how it starts. A machine with no session bus fails as unreachable, which is the truth.

use tidemark_ipc::DaemonProxy;

pub async fn daemon() -> zbus::Result<DaemonProxy<'static>> {
    let connection = zbus::Connection::session().await?;
    DaemonProxy::new(&connection).await
}
