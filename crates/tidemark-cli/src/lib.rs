//! `tidemarkctl`: the third consumer of the daemon's interface, after the window and
//! `busctl`.
//!
//! It performs no provider I/O — every number it prints came off the bus — and it holds no
//! runtime: zbus's async-io backend drives its own connection thread, so one `block_on` in
//! the binary is the whole of this program's concurrency.
//!
//! The library exists so the integration tests can drive the real command bodies against a
//! fake daemon they serve themselves. Testability comes from passing a proxy in, not from
//! an environment variable that redirects the bus name — a back door a user could trip
//! over.

pub mod cli;
pub mod connect;
pub mod exit;
pub mod format;
pub mod guard;
pub mod watch;
