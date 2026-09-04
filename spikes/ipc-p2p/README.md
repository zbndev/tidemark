# Tidemark Windows IPC transport spike

This crate is deliberately outside the repository workspace. It measures zbus 5.19 peer-to-peer
connections over real Windows AF_UNIX sockets without changing either product binary. The server
uses Tidemark's shipped bus name, object path, interface name, and `tidemark-types` wire values.
It is temporary and is removed by Windows-port todo 20.

Run the CI-runnable decision gates on Windows UCRT64:

```text
cargo test --manifest-path spikes/ipc-p2p/Cargo.toml -- --nocapture --test-threads=1
```

Build the three process surfaces, then run the local portion of VM gate G1:

```text
cargo build --manifest-path spikes/ipc-p2p/Cargo.toml --bins
cargo run --manifest-path spikes/ipc-p2p/Cargo.toml --bin ipc-p2p-g1
```

`ipc-p2p-g1` uses event barriers and process/connection completion only. It has no sleeps or
retry-delay polling. It exercises the product endpoint, non-ASCII and 107-byte paths, an expected
108-byte rejection, a same-SID process in a distinct Windows logon session, stale endpoints,
cancel/restart recovery, misleading-success guards, and 100 forced server restarts while Microsoft
Defender real-time protection is active.
