# Todo 10a - Windows secrets runtime CI gate

## Baseline

Before this change, the existing Windows jobs were compile-gap probes only. Run `33872298956`, artifact `probe-msvc.log` (cited by `task-10-windows-ci.md`), contains `cargo check --workspace` and no `cargo test ... secrets` invocation. Its Windows jobs therefore did not exercise Credential Manager or DPAPI at runtime.

## Change

`.github/workflows/ci.yml` adds `windows-tests`, gated to pushes of `feat/experimental-windows-native`, on the existing `windows-latest` runner and MSVC decision. It installs/logs target `x86_64-pc-windows-msvc`, then runs exactly:

```text
cargo test -p tidemark-core --target x86_64-pc-windows-msvc secrets -- --nocapture
```

The PowerShell step prints the command in Actions' run log, records the command exit code, and prints teardown/cleanup visibility. A nonzero cargo exit is propagated; this cannot be a compile-only green check.

## Validation

- `actionlint .github/workflows/ci.yml`: fails on the repository's pre-existing custom `ubuntu-26.04` runner label (actionlint reports it as unknown); no new diagnostic is reported for the added Windows job.
- Structural fallback: Python assertion verified `windows-tests`, `windows-latest`, and the exact runtime command; passed.
- `git diff --check`: passed.

## Landing manual QA

On the landed commit, inspect the real GitHub Actions `windows-tests` job: runner is Windows and logs show `rustup show`, `cargo --version`, `rustc -vV`, target `x86_64-pc-windows-msvc`, the exact command above, 13+ secrets tests including inline/DPAPI boundaries, test result/count, `secrets command exit code: 0`, and cleanup output. Confirm the job is not skipped for a push to `feat/experimental-windows-native`.

Adversarial probes: stale workflow state is addressed by checkout plus target/toolchain logging; dirty worktree is irrelevant on the fresh runner; misleading green output is prevented by propagating `$LASTEXITCODE` and using `cargo test`, not `cargo check`; long-command behavior is preserved as a direct shell invocation (no truncating pipeline). Unrelated adversarial classes (cross-user VM persistence, tamper/crash protocol, and credential-value leakage) belong to Todo 10's runtime tests/VM gate, not this workflow-only change.

## Cleanup

No local Cargo target or Windows credential artifacts were created. Worktree status was clean before editing; generated artifacts were not left behind. No push or merge performed.
