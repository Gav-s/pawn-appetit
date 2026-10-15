import {
    commands,
    events,
    type AnalysisNodeView,
    type AnalysisProjectSummary,
    type AnalysisTaskView,
    type AnalysisUpdate,
    type DeepAnalysisProgress,
    type SchedulerView,
    type DeepAnalysisProject,
    type DeepAnalysisResult,
    type DeepAnalysisTask,
} from "@/bindings";

export type {
    AnalysisNodeView,
    AnalysisProjectSummary,
    AnalysisTaskView,
    AnalysisUpdate,
    DeepAnalysisProgress,
    SchedulerView,
    DeepAnalysisProject,
    DeepAnalysisResult,
    DeepAnalysisTask,
} from "@/bindings";

async function unwrapCommand<T>(
    response: Promise<{ status: "ok"; data: T } | { status: "error"; error: string }>,
): Promise<T> {
    const result = await response;
    if (result.status === "error") throw new Error(result.error);
    return result.data;
}

function dispatchEvent<T>(callback: (payload: T) => void, payload: T): void {
    try {
        callback(payload);
    } catch (error) {
        console.error("Deep Analysis event handler failed", error);
    }
}

export interface DeepAnalysisService {
    listSummaries(): Promise<AnalysisProjectSummary[]>;
    getSummary(projectId: string): Promise<AnalysisProjectSummary>;
    getTree(projectId: string): Promise<AnalysisNodeView[]>;
    getNode(projectId: string, nodeId: string): Promise<AnalysisNodeView>;
    getTaskViews(projectId: string): Promise<AnalysisTaskView[]>;
    getScheduler(projectId: string): Promise<SchedulerView>;
    startAutomatic(projectId: string): Promise<void>;
    stop(projectId: string, clearPending: boolean): Promise<void>;
    manualProlong(projectId: string, nodeId: string, count: number): Promise<DeepAnalysisTask>;
    onUpdate(callback: (update: AnalysisUpdate) => void): Promise<() => void>;
    onProgress(callback: (progress: DeepAnalysisProgress) => void): Promise<() => void>;
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
    listSummaries: () => unwrapCommand(commands.listDeepAnalysisSummaries()),
    getSummary: (projectId) => unwrapCommand(commands.getDeepAnalysisSummary(projectId)),
    getTree: (projectId) => unwrapCommand(commands.getDeepAnalysisTree(projectId)),
    getNode: (projectId, nodeId) => unwrapCommand(commands.getDeepAnalysisNode(projectId, nodeId)),
    getTaskViews: (projectId) => unwrapCommand(commands.getDeepAnalysisTaskViews(projectId)),
    getScheduler: (projectId) => unwrapCommand(commands.getDeepAnalysisScheduler(projectId)),
    startAutomatic: async (projectId) => {
        await unwrapCommand(commands.startAutomaticDeepAnalysis(projectId));
    },
    stop: async (projectId, clearPending) => {
        await unwrapCommand(commands.stopDeepAnalysis(projectId, clearPending));
    },
    manualProlong: (projectId, nodeId, count) =>
        unwrapCommand(commands.manualProlongDeepAnalysis(projectId, nodeId, count)),
    onUpdate: (callback) =>
        events.analysisUpdate.listen((event) => dispatchEvent(callback, event.payload)),
    onProgress: (callback) =>
        events.deepAnalysisProgress.listen((event) => dispatchEvent(callback, event.payload)),
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
