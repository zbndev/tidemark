//! Everything that reaches outside the process: provider clients, the history database,
//! and secret storage.
//!
//! Consumed by `tidemarkd`. The GUI must never link this crate — see
//! `scripts/check-layering.sh`.

pub mod paths;

pub mod storage;

pub mod providers;

pub mod secrets;
