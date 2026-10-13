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

## Unfinished / next RED test

M1 is **not complete**. Deferred legacy tasks need an explicit occurrence-selection/replay workflow; the frontend still has no public tree read API or board-linked occurrence picker. Terminal mate/draw ordering, repeated-position and fifty-move history, and a full minimax rebuild still need focused tests. Bounded automatic expansion, priorities, green/red steering, multiple roots/engines, and imports remain later roadmap items. The existing Deep Analysis UI still shows the queue/results; new tree nodes/scores are backend-only and not yet visible there.

## Resume procedure

1. Load the `pawn-appetit-idea` skill; inspect Git status/log and read this file plus the roadmap. Do not reset uncommitted work or overwrite the user's app-data database.
2. Run Rust, frontend, formatting, lint and type checks above with the PATH/localStorage setup.
3. Start with one failing terminal/mate/draw or repetition-score test, implement the smallest slice, rerun checks, update this checkpoint and commit on `feature/deep-analysis`. Before pushing, inspect staged diffs for unrelated files/secrets and read back the remote branch/commit after the push.
