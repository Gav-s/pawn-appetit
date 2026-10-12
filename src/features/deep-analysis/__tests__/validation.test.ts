import { describe, expect, it } from "vitest";
import { validateDeepAnalysisProject } from "../validation";

const FEN = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

describe("deep analysis project validation", () => {
    it("rejects an empty name and malformed or illegal FEN before sending commands", () => {
        expect(validateDeepAnalysisProject("  ", FEN)).toBe("Enter a project name.");
        expect(validateDeepAnalysisProject("Opening", "not a FEN")).toBe(
            "Enter a valid FEN position.",
        );
        expect(validateDeepAnalysisProject("Opening", "8/8/8/8/8/8/8/8 w - - 0 1")).toBe(
            "Enter a valid FEN position.",
        );
        expect(validateDeepAnalysisProject("Opening", FEN)).toBeNull();
    });
});
