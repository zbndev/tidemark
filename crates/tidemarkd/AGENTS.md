# DAEMON KNOWLEDGE BASE

## OVERVIEW
Polling, credential orchestration, and IPC publication; score 9, a distinct runtime-owner domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Startup and shutdown | `src/main.rs` | Wires config, history, engine, publication, signals |
| Polls and account mutations | `src/engine.rs` | Single owner of accounts and history; largest module |
| D-Bus methods and login lifecycle | `src/service.rs` | `Daemon`, `DaemonState`, ordered `Published` mirror |
| Catalog and account construction | `src/registry.rs` | OAuth tables, keyed catalog, hand-written adapters |
| Poll timing | `src/scheduler.rs` | Pure `next_interval(Situation)` policy |
| Notifications | `src/notify.rs` | Pure decision/composition plus platform transports |
| Secrets adapter | `src/keyring.rs` | Implements the core `Secrets` interface |
| Startup preference effects | `src/startup.rs` | User service / Windows startup integration |
| Windows fan-out | `src/peer.rs` | Bounded per-peer queues; shared tests also run on Unix |
| Windows process resources | `src/lifecycle.rs`, `src/supervisor.rs` | Singleton, jobs, autostart, ConPTY |
| Release checks | `src/update.rs` | Compiled with `update-check` |

## CONVENTIONS
- `Command` plus one-shot replies serializes configuration mutations through `Engine`.
- Fetches use `JoinSet`; history ingest and account publication remain serialized by the owner.
- `Publication` carries engine changes to the service mirror; mutate the mirror before emitting signals.
- Account identity is `(provider, account)`; preserve configured vector order and keep `default` first.
- Rename/removal copies credentials and rekeys history before the config durability point.
- Roll back pre-commit migration failures; post-commit cleanup failures do not undo reported success.
- Service identity locks and login cancellation prevent stale writes under retired account IDs.
- Notification delivery is recorded only after transport acceptance; a failed send remains retryable.
- `PeerHub` uses `try_send`: evict an entire full/closed peer rather than blocking publication.

## ANTI-PATTERNS
- Do not infer post-mutation topology from a potentially lagging `Published` snapshot; use the engine reply.
- Do not collapse locked/unavailable keyrings into ordinary provider failures.
- Do not read pasted-session slots for accounts selecting browser authentication.
- Do not shorten a provider's longer `Retry-After` or tightly retry user-action states.
- Do not open Windows history before acquiring the singleton.
- Do not use endpoint-file existence as proof a Windows socket is live; probe before stale-path removal.
- Do not await clients under the peer-hub lock or drop individual queued announcements.
- Do not leave Task Scheduler battery restrictions or its default execution limit enabled.

## CHECKS
- `cargo test -p tidemarkd engine::tests` and `cargo test -p tidemarkd service::tests` cover the largest mutation paths.
- Use `dbus-run-session -- cargo test -p tidemarkd` for an isolated Unix bus.
- `cargo test -p tidemarkd --features update-check` includes release-check behavior.
- `cargo run -p tidemarkd -- --version` exercises the binary without starting polling.
- Windows-only lifecycle/ConPTY checks need a Windows toolchain and deliberate platform QA; Linux cannot verify those branches.
