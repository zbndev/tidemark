//! One module per entity the daemon owns. Every function takes the proxy rather than
//! building one, which is what makes them testable against a fake daemon.

pub mod account;
pub mod auth;
pub mod config;
pub mod plugin;
pub mod provider;
