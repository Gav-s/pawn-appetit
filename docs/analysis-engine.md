# Deep Analysis implementation

Pawn Appétit uses its existing Rust/Tauri backend, Shakmaty rules, local UCI process, SQLite store and React UI. This is an independent implementation, not a port of another application's code or storage. The desktop screen and the debug CLI share the persisted analysis core.

## Boundaries

- `src-tauri/src/chess/`: legal chess positions, PGN support, and UCI process (`uci`, `isready`, `position fen ... [moves ...]`, `go infinite`, `stop`, `quit`). `deep_analysis.rs` validates scored `info` PVs and parses `score cp`/`score mate`, depth and MultiPV. Existing option discovery and MultiPV setting are reused.
- `deep_analysis_score.rs`: white-relative `Centipawns`, signed `Mate(distance)` and `Unknown`. Mate is never converted to a centipawn. The separate tree index is the first of 100 supplied upper bounds greater than or equal to a centipawn score, or 100 beyond them; index 50 corresponds to zero centipawns. Mate and unknown have no tree index. Minimax ranks a White win by mate ahead of all centipawns and a White loss behind all centipawns; White maximizes, Black minimizes.
- `deep_analysis_scheduler.rs`: typed settings, completion policy and four candidate-selection stages. These strategies are bounded heuristics over known, legal engine-PV occurrences, not a recreation of Aquarium's candidate-generation internals.
- `deep_analysis_tree.rs`: move _occurrences_ with parent edges and complete move histories; a repeated position can have multiple distinct paths. Direct white-relative score and provisional backed-up minimax score are separate. Analyzed result, PV edges and ancestor updates are committed in one SQLite transaction.
- `deep_analysis.rs`: persisted project/task queue, scheduler cycle, conservative cache, UCI worker, Tauri progress and semantic update events, commands, and CLI entry points. Automatic mode is opt-in and does not change older manually queued projects.
- `deep_analysis_task.rs` and `deep_analysis_read.rs`: immutable queued-task execution requirements and stable UI-facing project, node, scheduler and task metadata. The React screen subscribes to the service's progress/update events and refreshes typed read models; a 15-second timer is a fallback, not the normal update path. The event-only test disables fallback timers and verifies the refresh.

## Scheduler

Cycle `1 → Random`, `2 → Alternatives`, `3 → Base or Prolong`; repeat. On every third cycle choose Prolong when `(cycle / 3) % max(1, prolongFrequency) == 0`, otherwise Base. A run of empty stages spanning a configured frequency interval exhausts an otherwise empty queue. No timer or engine is restarted on database open.

Base chooses unanalysed, known-PV occurrences near the root's backed-up centipawn index (`± maxDelta`), widens by three through an additional 52, clamps candidates to `0…99`, and limits _this stage's_ output to `maxPositions`. Random first filters by range, then samples without replacement with weight favoring indices closer to root; seeded RNG makes the selector reproducible in tests. Alternatives selects non-primary sibling PV moves with a meaningful analyzed parent and a close score. Prolong selects primary continuations; `prolongDepth` is a count of _position tasks_, not a UCI depth. A completed prolong task transactionally queues at most one immediate legal PV child with the remaining count decremented. A manual prolong can be queued for an unanalysed PV child and enters the priority lane; it does not interrupt the active task. Automatic search is bounded by `maxPly` relative to root, plus `maxPositions` per stage. Future work should make tree width and total CPU budget configurable independently.

The default automated worker uses `go infinite` and accepts a valid scored PV under the typed modes: TimeOrDepth (`time >= minimum || depth >= requested`), TimeAndDepth (`time >= minimum && depth >= requested`), Ungated (valid PV). At ten times requested time it accepts a valid PV even without depth or returns an explicit timeout with no accepted result. The worker sends `stop`, waits briefly for `bestmove`, and shuts the process down. Engine EOF, no scored PV and invalid PV fail the task rather than inventing an evaluation. `time_ms` is milliseconds, depth is plies, and the hard limit is independent of the engine's `go` arguments.

## Persistence and recovery

The separate `deep-analysis.sqlite` file holds projects, tasks, scored lines, move-occurrence tree, task-to-node links, the `deep_scheduler` row (`settings` JSON, `cycle`, empty-stage count and enabled flag), and `deep_analysis_cache`. The task-execution migration sets `PRAGMA user_version=3`, adds non-null execution columns to existing tasks, backfills legacy rows once from available project/scheduler settings (historical settings cannot be reconstructed exactly), and installs an insert trigger that snapshots new tasks. Reopening is idempotent and does not drop existing tasks/results; downgrade support is not implemented. Every queued task retains its engine/profile, MultiPV, requested time/depth, operation and cache policy; strategy, priority, source position/occurrence and remaining prolong count remain task-specific. An accepted result's lines, cache entry, tree/PV path, minimax and optional prolong successor are committed in one transaction. Cache keys use canonical legal FEN plus occurrence history (conservative repetition safety), engine path and MultiPV count; both measured search time and depth must meet requested criteria. It does not yet share results across distinct transposition histories.

On app startup recovery requeues interrupted running tasks and moves persisted `running` or `pausing` projects to `paused`. No worker is relaunched until explicit Resume. Pause after current marks the project `pausing`: the active search reaches its ordinary acceptance condition, its result/tree/minimax commit completes, then the worker pauses before claiming another task; queue and cycle remain intact. Immediate Pause signals cancellation through the active worker, stops the engine, requeues the interrupted task, and retains a depth-improving provisional PV snapshot without accepting it as completed. Stop disables generation and can retain or clear pending work without deleting accepted results. Paused and stopped projects retain their tree, cache, scheduler settings and queued tasks. The CLI `run` stops after its accepted-result budget and pauses; `show` only reads; `resume` explicitly restarts work. Reopening SQLite alone never launches Stockfish.

## Try the core

From the repository root, with local Stockfish installed:

    PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo run --manifest-path src-tauri/Cargo.toml --no-default-features --bin analysis_debug -- run "$HOME/.hermes/cache/scratch/my-analysis.sqlite" /opt/homebrew/bin/stockfish 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1' 4 150

Copy the printed project UUID, then independently run `analysis_debug show DB PROJECT_ID` or `analysis_debug resume DB PROJECT_ID COUNT`. Do not use a user's existing app-data database for this smoke test.

Checks: `cargo test --manifest-path src-tauri/Cargo.toml --no-default-features`, `cargo fmt --manifest-path src-tauri/Cargo.toml --check`, `cargo clippy --manifest-path src-tauri/Cargo.toml --no-default-features --all-targets`, and frontend Vitest/TypeScript checks.

## Deliberate differences and remaining work

No proprietary binary resources, Delphi layouts, `.elm`/`.hsh`/`.dst` formats or `insert_best` behavior are used. The exact historic Alternatives/Random/Prolong scoring, mate index and pause path were unresolved: our tested heuristics and explicit pause semantics are deliberate independent choices. Tree occurrences are not merged merely because FEN matches; cache reuse is conservative. The provisional minimax does not prove an incomplete tree is solved.

Deferred beyond this milestone: shared-transposition position records, terminal/draw proofs and additional engine statistics. The debug CLI remains useful for bounded recovery tests; the Rust/Tauri/React screen is the supported desktop workflow.
