//! Native installation support; provider policy remains in the daemon binary.

pub mod installation;
#[cfg(windows)]
pub mod installer_integration;
#[cfg(windows)]
pub mod installer_process;
#[cfg(windows)]
pub mod lifecycle;
