//! Everything that reaches outside the process: provider clients, the history database,
//! and secret storage.
//!
//! Consumed by `tidemarkd`. The GUI must never link this crate — see
//! `scripts/check-layering.sh`.

pub mod providers {
    //! One module per provider, behind a common trait. Filled in from Step 3 onward.
}

pub mod storage {
    //! SQLite history, keyed `(provider, account, window, segment)`. Filled in at Step 2.
}

pub mod secrets {
    //! Secret Service access for keys Tidemark owns, and the read/refresh/write-back path
    //! for third-party CLI credential files described in ADR 0001.
}
