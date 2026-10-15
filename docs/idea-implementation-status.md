# IDeA implementation checkpoint

This is the Git-tracked resume point for the user-local Hermes skill `pawn-appetit-idea`. Read `docs/idea-roadmap.md`, inspect current `git status --short --branch` and rerun checks; do not assume the working tree matches this file.

## Environment / branch

- Repository: `~/Projects/pawn-appetit`, branch `feature/deep-analysis` in fork `Gav-s/pawn-appetit`. The roadmap was published at `8a7a03fd`; inspect newer commits before editing. Do not merge into `main` without a request.
- macOS arm64: Homebrew `rustup` and stable Cargo 1.99.0 were already installed, but its keg-only proxy directory was absent from PATH. `~/.zshrc` now prepends `/opt/homebrew/opt/rustup/bin`; noninteractive commands can run `export PATH="/opt/homebrew/opt/rustup/bin:$PATH"` first.
- Node 26 Vitest needs `NODE_OPTIONS="--localstorage-file=$HOME/.hermes/cache/scratch/pawn-vitest-localstorage"` to make localStorage available for route imports.
- `pnpm` is not on this tool shell's PATH. The native dev app launches with `./node_modules/.bin/tauri dev --config '{"build":{"beforeDevCommand":"npm run start-vite"}}'`. This overrides only the frontend dev command; it does not edit the project's config. Stop a previous dev session before launching another.

## Implemented: first M1 backend slice

- `src-tauri/src/deep_analysis_tree.rs` creates persistent SQLite occurrence nodes with parent move, canonical FEN, full move history, ply, separate direct and provisional minimax score kind/value, direct depth, and task provenance. Paths are not coalesced by FEN (repetition depends on move history). `Store::save` persists completed PV results/tree paths atomically and propagates white-relative scores upward, with White selecting maximum and Black minimum among scored children. A direct evaluation remains separate from backed-up values.
- `deep_tasks.occurrence_id` anchors an analyzed continuation to its exact move occurrence. Queueing without an occurrence ID succeeds only when the completed parent's PV identifies a unique path; same-ply transpositions are rejected as ambiguous rather than silently misattached. An explicit occurrence must actually lie on that named parent's PV. The generated Tauri binding accepts `occurrenceId`; the frontend does not yet expose a tree picker/read API.
- `deep_tree_analyses` records *all* completed task-to-node links, including duplicate analyses of one occurrence; the direct score and its source task come from the newest linked task by task insertion order. Reopening does not replay already linked tasks or replace a newer direct score with an older orphan. Existing completed results migrate on open without deleting old rows. Before replaying an unlinked legacy continuation, migration clears a potentially wrong cached one-to-one pointer inside a savepoint, then attaches an unambiguous position to its real occurrence. Malformed or ambiguous legacy tasks are recorded in `deep_tree_backfill_deferred` instead of blocking app startup; their original task/result rows remain accessible, but their occurrence cannot be inferred automatically. A PV that would exceed `u32::MAX` ply returns an error with no partial tree/results write.
- Tests cover a stored/reopened path, White/Black provisional minimax, basic legacy backfill, ambiguous transpositions, explicit occurrence validation, duplicate analyses across repeated opens, deferred legacy results, wrong cached child links, ply overflow rollback, and stale guessed-score cleanup. Reviewer-found occurrence and migration bugs were reproduced by focused RED tests and fixed.

## Verification (13 October 2026)

- `cargo test --manifest-path src-tauri/Cargo.toml --no-default-features`: 105 passed, 9 ignored, 0 failed; two pre-existing dead-code warnings. Before this slice the baseline was 91 passed, 9 ignored.
- `NODE_OPTIONS="--localstorage-file=$HOME/.hermes/cache/scratch/pawn-vitest-localstorage" ./node_modules/.bin/vitest run`: 187 passed in 25 files. `./node_modules/.bin/tsc --noEmit`: passed.
- `cargo fmt --manifest-path src-tauri/Cargo.toml --check` and `git diff --check`: passed.
- `cargo clippy --manifest-path src-tauri/Cargo.toml --no-default-features --all-targets`: exits 0 with existing project-wide warnings and no diagnostics in the touched Deep Analysis modules. Strict `-- -D warnings` fails on unrelated baseline warnings.
- The native `tauri dev` app launched; its main window was frontmost and Vite returned HTTP 200. The existing app-data `deep-analysis.sqlite` passed `PRAGMA integrity_check` after schema migration; a prelaunch SQLite backup was saved locally under `~/.hermes/cache/scratch/`. No real-engine search or interactive board/IDeA smoke test has been performed for this slice. The running dev process may stop when its terminal session ends; check live process state before promising it is open.

## Verified first automatic core slice (15 October 2026)

- Branch `feature/deep-analysis`, commits `e5108587` (score), `744ea640` (typed scheduler policy), `3f651071` (automatic SQLite/UCI worker and CLI). These are local commits; inspect `git status` and remote before any push.
- New independent score type retains white-relative centipawns/mate/unknown. The supplied 100 threshold bounds produce 101 possible centipawn indices; tests cover all boundaries and zero → 50. Mate remains separately ranked for provisional minimax.
- Project creation now persists an unanalysed root. Opt-in scheduler settings and cycle survive reopen; bounded Random, Alternatives, Base and Prolong select existing legal PV occurrences. Manual priority and a tested 3-position prolong chain exist in the backend; the manual-prolong function has no Tauri/UI binding yet. `go infinite`, valid-score gates and hard timeout replace `go movetime` in the deep-analysis worker; interactive engine behavior is unchanged. Accepted results, cache, tree/minimax and prolong successor share a transaction. The cache conservatively keys full position plus history/engine/MultiPV and requires measured time and depth.
- Real `/opt/homebrew/bin/stockfish` was installed and run with the CLI debug slice. Database: `~/.hermes/cache/scratch/idea-real-stockfish-v2.sqlite`, project `47f92dfe-4682-4d4e-8f29-31b21941fc81`. Four completed searches created 63 persisted nodes; a separate `show` read reproduced the same root score and stage. Explicit `resume` completed two more searches; a new `show` read found six completed tasks, cycle 5 and 101 nodes. Root direct score 24 cp and provisional minimax 42 cp after resume. No proprietary executable/engine was run.
- `cargo test --manifest-path src-tauri/Cargo.toml --no-default-features`: 124 passed, 9 ignored, 0 failed. Vitest: 187 passed/25 files; `tsc --noEmit`, `cargo fmt --check`, `git diff --check`, and baseline-warning-tolerant Clippy all exited 0. Clippy reports pre-existing project-wide warnings and a few new style/dead-code warnings in Deep Analysis; no compile errors.
- CLI: `analysis_debug run DB ENGINE 'FEN' COUNT TIME_MS`, `show DB PROJECT_ID`, and explicit `resume DB PROJECT_ID COUNT`. See `docs/analysis-engine.md` for architecture, units, limitations and exact command.

## Unfinished / next RED test

This is a working core/CLI slice, not full IDeA or a Swift app. The existing app is Rust/Tauri and was extended without duplicating Shakmaty, UCI or SQLite. A native SwiftUI frontend was not built. Next: typed tree read and task metadata API; explicit stop retain/clear and finish-current pause; task-specific time/depth/engine/operation persisted fields; store UCI `nodes`/`time`; root-level manual prolong; shared transposition identity and completion semantics; crash-recovery CLI; UI board-linked tree. Do not represent the current status/queue page as that UI. No further Aquarium reverse engineering is required for these design choices.

## Resume procedure

1. Load the `pawn-appetit-idea` skill; inspect Git status/log, branch and this checkpoint. Do not reset unrelated changes or overwrite the user's app-data database.
2. Run the Rust, Vitest, TypeScript, format and Clippy checks above; use `PATH=/opt/homebrew/opt/rustup/bin:$PATH` and `NODE_OPTIONS=--localstorage-file=$HOME/.hermes/cache/scratch/pawn-vitest-localstorage`.
3. Write a failing typed tree-read/API or explicit stop behavior test, implement the smallest slice, verify the real DB and CLI, update this checkpoint, then commit scoped paths. Never merge or push without verifying the target and request.

