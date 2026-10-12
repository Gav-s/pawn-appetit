import { Chess } from "chessops/chess";
import { parseFen } from "chessops/fen";

export function validateDeepAnalysisProject(name: string, fen: string): string | null {
    if (!name.trim()) return "Enter a project name.";
    const [setup] = parseFen(fen.trim()).unwrap(
        (value) => [value, null],
        () => [null, null],
    );
    if (!setup || Chess.fromSetup(setup).isErr) return "Enter a valid FEN position.";
    return null;
}
