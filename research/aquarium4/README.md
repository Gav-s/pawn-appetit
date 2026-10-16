# Aquarium 4 behavioural reference — clean-room boundary

These notes describe externally observable chess-analysis behaviour for comparison only. Pawn Appétit's implementation is independently designed and written using its own chess model, UCI worker, SQLite schema, tests and UI. The reports in `/Users/gavinsinnerton/aquarium-extracted/analysis` are research references, not code or database specifications; they are deliberately outside this Git repository.

Do not copy ChessOK/Aquarium Delphi code, disassembly, binary layouts, resources, engine bundles, database files or proprietary formats into this repository. Do not commit the Aquarium executable or extracted artifacts. Unknown historic behavior (notably `insert_best`, mate-index treatment, persistence and pause details) is not silently guessed: choose and test a safe independent behavior instead.

Current implementation and its remaining limits: `docs/analysis-engine.md`. Transposition-aware shared position storage is a deliberate clean-room improvement; this documentation does not claim to reproduce Aquarium's unknown transposition internals.
