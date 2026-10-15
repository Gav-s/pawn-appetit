import { describe, expect, it, vi } from "vitest";
import { deepAnalysisService } from "../service";
import { commands, events } from "@/bindings";

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
        listDeepAnalysisSummaries: vi.fn(),
        getDeepAnalysisSummary: vi.fn(),
        getDeepAnalysisTree: vi.fn(),
        getDeepAnalysisNode: vi.fn(),
        getDeepAnalysisTaskViews: vi.fn(),
        getDeepAnalysisScheduler: vi.fn(),
        startAutomaticDeepAnalysis: vi.fn(),
        stopDeepAnalysis: vi.fn(),
        manualProlongDeepAnalysis: vi.fn(),
    },
    events: {
        analysisUpdate: { listen: vi.fn() },
        deepAnalysisProgress: { listen: vi.fn() },
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

    it("uses typed project, tree, task and scheduler reads and explicit automatic/stop/prolong actions", async () => {
        vi.mocked(commands.listDeepAnalysisSummaries).mockResolvedValue(
            ok([{ id: "p1" }]) as never,
        );
        vi.mocked(commands.getDeepAnalysisSummary).mockResolvedValue(ok({ id: "p1" }) as never);
        vi.mocked(commands.getDeepAnalysisTree).mockResolvedValue(ok([{ id: "1" }]) as never);
        vi.mocked(commands.getDeepAnalysisNode).mockResolvedValue(ok({ id: "1" }) as never);
        vi.mocked(commands.getDeepAnalysisTaskViews).mockResolvedValue(ok([]) as never);
        vi.mocked(commands.getDeepAnalysisScheduler).mockResolvedValue(
            ok({ state: "paused" }) as never,
        );
        vi.mocked(commands.startAutomaticDeepAnalysis).mockResolvedValue(ok(null) as never);
        vi.mocked(commands.stopDeepAnalysis).mockResolvedValue(ok(null) as never);
        vi.mocked(commands.manualProlongDeepAnalysis).mockResolvedValue(ok({ id: "t2" }) as never);
        expect(await deepAnalysisService.listSummaries()).toEqual([{ id: "p1" }]);
        expect(await deepAnalysisService.getSummary("p1")).toEqual({ id: "p1" });
        expect(await deepAnalysisService.getTree("p1")).toEqual([{ id: "1" }]);
        expect(await deepAnalysisService.getNode("p1", "1")).toEqual({ id: "1" });
        expect(await deepAnalysisService.getTaskViews("p1")).toEqual([]);
        expect(await deepAnalysisService.getScheduler("p1")).toEqual({ state: "paused" });
        await deepAnalysisService.startAutomatic("p1");
        await deepAnalysisService.stop("p1", false);
        await deepAnalysisService.manualProlong("p1", "1", 2);
        expect(commands.getDeepAnalysisNode).toHaveBeenCalledWith("p1", "1");
        expect(commands.startAutomaticDeepAnalysis).toHaveBeenCalledWith("p1");
        expect(commands.stopDeepAnalysis).toHaveBeenCalledWith("p1", false);
        expect(commands.manualProlongDeepAnalysis).toHaveBeenCalledWith("p1", "1", 2);
    });

    it("subscribes to semantic analysis updates and unlistens on teardown", async () => {
        const unlisten = vi.fn();
        const callback = vi.fn();
        vi.mocked(events.analysisUpdate.listen).mockResolvedValue(unlisten);
        const detach = await deepAnalysisService.onUpdate(callback);
        const handle = vi.mocked(events.analysisUpdate.listen).mock.calls[0][0];
        handle({ payload: { projectId: "p1", kind: "task_completed" } } as never);
        expect(callback).toHaveBeenCalledWith({ projectId: "p1", kind: "task_completed" });
        detach();
        expect(unlisten).toHaveBeenCalledOnce();
    });

    it("keeps a listener usable after one callback throws", async () => {
        const callback = vi.fn().mockImplementationOnce(() => {
            throw new Error("render failed");
        });
        const log = vi.spyOn(console, "error").mockImplementation(() => {});
        try {
            vi.mocked(events.analysisUpdate.listen).mockResolvedValue(vi.fn());
            const detach = await deepAnalysisService.onUpdate(callback);
            const handle = vi.mocked(events.analysisUpdate.listen).mock.lastCall![0];
            const event = {
                payload: { projectId: "p1", kind: "tree_node_updated", taskId: "t1" },
            } as Parameters<typeof handle>[0];
            expect(() => handle(event)).not.toThrow();
            handle(event);
            expect(callback).toHaveBeenCalledTimes(2);
            expect(log).toHaveBeenCalledOnce();
            detach();
        } finally {
            log.mockRestore();
        }
    });

    it("subscribes to progress and unlistens on teardown", async () => {
        const unlisten = vi.fn();
        const callback = vi.fn();
        vi.mocked(events.deepAnalysisProgress.listen).mockResolvedValue(unlisten);
        const detach = await deepAnalysisService.onProgress(callback);
        const handle = vi.mocked(events.deepAnalysisProgress.listen).mock.calls[0][0];
        handle({ payload: { projectId: "p1" } } as never);
        expect(callback).toHaveBeenCalledWith({ projectId: "p1" });
        detach();
        expect(unlisten).toHaveBeenCalledOnce();
    });

    it("surfaces typed command errors rather than reporting a successful action", async () => {
        vi.mocked(commands.pauseDeepAnalysis).mockResolvedValue({
            status: "error",
            error: "engine stopped",
        });
        await expect(deepAnalysisService.pause("p1")).rejects.toThrow("engine stopped");
    });
});
