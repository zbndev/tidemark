# STORAGE KNOWLEDGE BASE

## OVERVIEW
Transactional history, reset segmentation, retention, and notification deduplication; score 8, distinct persistence domain.

## WHERE TO LOOK
| Task | Location | Notes |
|------|----------|-------|
| Database opening/ingest | `mod.rs` | `History`, `IngestReport`, `WindowOutcome` |
| Retention and queries | `mod.rs` | Thinning, account operations, notice state |
| Schema changes | `schema.rs` | `CURRENT_VERSION`, ordered migration steps |
| Reset classification | `segment.rs` | `Observation`, `Boundary`, `classify` |
| Recorded-history regression | `../../tests/corpus_replay.rs` | Segmentation against corpus |
| Provider-to-database integration | `../../tests/provider_to_history.rs` | Snapshot ingestion contract |

## CONVENTIONS
- History identity includes `(provider, account, window, segment)` throughout.
- File-backed databases use WAL; preparation enables foreign keys and NORMAL synchronous mode.
- Ingest is transactional across a snapshot's windows.
- Non-newer timestamps go into `IngestReport::stale` and do not rewind state.
- `window_state` tracks every observed reading, independently from the last retained point.
- Store changes in consumption, new segments, and flat-window anchors at hourly intervals.
- Full resolution lasts 90 days; older data uses 15-minute buckets while preserving segment edges.
- A consumption drop greater than 0.5 percentage points starts a new segment.
- A reset jump starts a segment only when it exceeds elapsed time plus 300 seconds of jitter.
- Missing reset timestamps do not disable usage-drop segmentation.
- Notifications key their deduplication to segments; classification changes affect more than charts.
- Rekey refuses occupied destination accounts instead of merging histories implicitly.

## ANTI-PATTERNS
- Never edit a shipped migration in place; append a step and advance the schema version.
- Do not split segments merely because `resets_at` changed: rolling resets drift with the clock.
- Do not classify against only the last written point; an unretained observation can reveal a rollover.
- Do not omit the account dimension from queries, notices, or deletion/rekey operations.
- Do not interpret `stored == false` as an absent observation.

## COMMANDS
```bash
cargo test -p tidemark-core --lib storage
cargo test -p tidemark-core --test corpus_replay
cargo test -p tidemark-core --test provider_to_history
```
