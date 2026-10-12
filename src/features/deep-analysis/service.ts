import {
    commands,
    type DeepAnalysisProject,
    type DeepAnalysisResult,
    type DeepAnalysisTask,
} from "@/bindings";

export type { DeepAnalysisProject, DeepAnalysisResult, DeepAnalysisTask } from "@/bindings";

async function unwrapCommand<T>(
    response: Promise<{ status: "ok"; data: T } | { status: "error"; error: string }>,
): Promise<T> {
    const result = await response;
    if (result.status === "error") throw new Error(result.error);
    return result.data;
}

export interface DeepAnalysisService {
    listProjects(): Promise<DeepAnalysisProject[]>;
    listTasks(projectId: string): Promise<DeepAnalysisTask[]>;
    listResults(projectId: string): Promise<DeepAnalysisResult[]>;
    createProject(
        title: string,
        engine: string,
        timeMs: number,
        multipv: number,
        fen: string,
        moves: string[],
    ): Promise<DeepAnalysisProject>;
    queueRoot(projectId: string, fen: string): Promise<DeepAnalysisTask>;
    start(projectId: string): Promise<void>;
    pause(projectId: string): Promise<void>;
    cancel(projectId: string): Promise<void>;
    resume(projectId: string): Promise<void>;
}

export const deepAnalysisService: DeepAnalysisService = {
    listProjects: () => unwrapCommand(commands.listDeepAnalysisProjects()),
    listTasks: (projectId) => unwrapCommand(commands.listDeepAnalysisTasks(projectId)),
    listResults: (projectId) => unwrapCommand(commands.listDeepAnalysisResults(projectId)),
    createProject: (title, engine, timeMs, multipv, fen, moves) =>
        unwrapCommand(
            commands.createDeepAnalysisProject(title, engine, timeMs, multipv, fen, moves),
        ),
    queueRoot: (projectId, fen) => unwrapCommand(commands.queueDeepAnalysisRoot(projectId, fen, 0)),
    start: async (projectId) => {
        await unwrapCommand(commands.startDeepAnalysis(projectId));
    },
    pause: async (projectId) => {
        await unwrapCommand(commands.pauseDeepAnalysis(projectId));
    },
    cancel: async (projectId) => {
        await unwrapCommand(commands.cancelDeepAnalysis(projectId));
    },
    resume: async (projectId) => {
        await unwrapCommand(commands.resumeDeepAnalysis(projectId));
    },
};
