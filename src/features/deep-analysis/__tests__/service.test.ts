import { describe, expect, it, vi } from "vitest";
import { deepAnalysisService } from "../service";
import { commands } from "@/bindings";

vi.mock("@/bindings", () => ({
    commands: {
        createDeepAnalysisProject: vi.fn(),
        listDeepAnalysisProjects: vi.fn(),
        listDeepAnalysisTasks: vi.fn(),
        listDeepAnalysisResults: vi.fn(),
        queueDeepAnalysisRoot: vi.fn(),
        startDeepAnalysis: vi.fn(),
        pauseDeepAnalysis: vi.fn(),
        cancelDeepAnalysis: vi.fn(),
        resumeDeepAnalysis: vi.fn(),
    },
}));

const ok = (data: unknown) => ({ status: "ok", data });

describe("typed deep analysis command adapter", () => {
    it("passes the root FEN and time budget atomically to project creation and reads persisted result lines", async () => {
        vi.mocked(commands.createDeepAnalysisProject).mockResolvedValue(ok({ id: "p1" }) as never);
        vi.mocked(commands.queueDeepAnalysisRoot).mockResolvedValue(ok({ id: "t1" }) as never);
        vi.mocked(commands.listDeepAnalysisResults).mockResolvedValue(
            ok([
                {
                    taskId: "t1",
                    lines: [
                        {
                            multipv: 1,
                            depth: 18,
                            scoreKind: "cp",
                            scoreValue: 37,
                            uciMoves: ["e2e4"],
                        },
                    ],
                },
            ]) as never,
        );
        await deepAnalysisService.createProject(
            "Study",
            "/usr/bin/stockfish",
            10000,
            2,
            "startpos",
            [],
        );
        expect(commands.createDeepAnalysisProject).toHaveBeenCalledWith(
            "Study",
            "/usr/bin/stockfish",
            10000,
            2,
            "startpos",
            [],
        );
        await deepAnalysisService.queueRoot("p1", "startpos");
        expect(commands.queueDeepAnalysisRoot).toHaveBeenCalledWith("p1", "startpos", 0);
        expect(await deepAnalysisService.listResults("p1")).toEqual([
            {
                taskId: "t1",
                lines: [
                    { multipv: 1, depth: 18, scoreKind: "cp", scoreValue: 37, uciMoves: ["e2e4"] },
                ],
            },
        ]);
    });

    it("sends cancellation and surfaces backend errors", async () => {
        vi.mocked(commands.cancelDeepAnalysis).mockResolvedValue({ status: "ok", data: null });
        await deepAnalysisService.cancel("p1");
        expect(commands.cancelDeepAnalysis).toHaveBeenCalledWith("p1");
        vi.mocked(commands.cancelDeepAnalysis).mockResolvedValue({
            status: "error",
            error: "already cancelled",
        });
        await expect(deepAnalysisService.cancel("p1")).rejects.toThrow("already cancelled");
    });

    it("surfaces typed command errors rather than reporting a successful action", async () => {
        vi.mocked(commands.pauseDeepAnalysis).mockResolvedValue({
            status: "error",
            error: "engine stopped",
        });
        await expect(deepAnalysisService.pause("p1")).rejects.toThrow("engine stopped");
    });
});
