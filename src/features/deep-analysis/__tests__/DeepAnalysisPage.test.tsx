import { MantineProvider } from "@mantine/core";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import DeepAnalysisPage from "../DeepAnalysisPage";
import type {
  AnalysisNodeView,
  AnalysisProjectSummary,
  AnalysisTaskView,
  SchedulerView,
  DeepAnalysisProgress,
  AnalysisUpdate,
  DeepAnalysisService,
} from "../service";

const FEN = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const AFTER_E4_FEN = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
const project = {
  id: "p1",
  title: "Opening study",
  engine: "/usr/bin/stockfish",
  timeMs: 10000,
  multipv: 2,
  status: "paused",
  createdAt: "2026-01-01",
};
const task = {
  id: "t1",
  projectId: "p1",
  parentTaskId: null,
  fen: FEN,
  moves: [],
  ply: 0,
  status: "queued",
  error: null,
};
const line = {
  multipv: 1,
  depth: 18,
  scoreKind: "cp",
  scoreValue: 37,
  scorePerspective: "white",
  uciMoves: ["e2e4", "e7e5"],
};

const summary: AnalysisProjectSummary = {
  id: "p1",
  name: "Opening study",
  rootFen: FEN,
  schedulerState: "paused",
  currentStage: "base",
  engine: "/usr/bin/stockfish",
  createdAt: "2026-01-01",
  updatedAt: null,
  taskCounts: { queued: 1, running: 0, completed: 0, failed: 0, cancelled: 0 },
  analysedNodeCount: 0,
};
const rootNode: AnalysisNodeView = {
  id: "1",
  parentId: null,
  positionId: "1",
  transpositionCount: 1,
  moveUci: null,
  moveSan: null,
  fen: FEN,
  sideToMove: "white",
  rawEvaluation: { kind: "unknown" },
  treeIndex: null,
  depth: null,
  elapsedMs: null,
  pv: [],
  engine: "/usr/bin/stockfish",
  minimaxEvaluation: { kind: "unknown" },
  status: "queued",
  transpositionLinked: false,
};
const scheduler: SchedulerView = {
  state: "paused",
  cycle: "0",
  stage: "base",
  queueLength: 1,
  activeTaskId: null,
  completedCount: 0,
  emptyStageCount: 0,
  exhausted: false,
  engineState: "idle",
};

Object.assign(globalThis, {
  IS_REACT_ACT_ENVIRONMENT: true,
  ResizeObserver: class {
    observe() {}
    unobserve() {}
    disconnect() {}
  },
});
Element.prototype.scrollIntoView = vi.fn();
Object.defineProperty(window, "matchMedia", {
  writable: true,
  value: vi.fn().mockImplementation((query) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })),
});
let root: Root | undefined;
let host: HTMLDivElement;
afterEach(async () => {
  if (root) {
    const mountedRoot = root;
    await act(async () => mountedRoot.unmount());
    root = undefined;
  }
  host?.remove();
  vi.restoreAllMocks();
});

async function render(
  service: DeepAnalysisService,
  engines = [{ name: "Stockfish", path: "/usr/bin/stockfish" }],
) {
  host = document.createElement("div");
  document.body.append(host);
  const mountedRoot = createRoot(host);
  root = mountedRoot;
  await act(async () =>
    mountedRoot.render(
      <MantineProvider>
        <DeepAnalysisPage service={service} installedEngines={engines} />
      </MantineProvider>,
    ),
  );
}
async function change(label: string, value: string) {
  const input = document.querySelector<HTMLInputElement>(`[aria-label="${label}"]`)!;
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}
async function click(label: string) {
  const button = [...document.querySelectorAll("button")].find(
    (node) => node.textContent?.trim() === label,
  )!;
  expect(button, `button ${label}`).toBeTruthy();
  await act(async () => button.click());
}

function fixture(persisted = false) {
  let projects: (typeof project)[] = persisted ? [project] : [];
  let tasks: (typeof task)[] = persisted ? [task] : [];
  let results: { taskId: string; lines: (typeof line)[] }[] = persisted
    ? [{ taskId: task.id, lines: [line] }]
    : [];
  const listeners = new Set<(update: AnalysisUpdate) => void>();
  let latestListener: ((update: AnalysisUpdate) => void) | undefined;
  const service: DeepAnalysisService & {
    emitUpdate(update: AnalysisUpdate): void;
    updateListenerCount(): number;
    latestUpdateListener(): ((update: AnalysisUpdate) => void) | undefined;
  } = {
    emitUpdate: (update) => {
      for (const listener of listeners) listener(update);
    },
    updateListenerCount: () => listeners.size,
    latestUpdateListener: () => latestListener,
    onUpdate: vi.fn(async (callback) => {
      latestListener = callback;
      listeners.add(callback);
      return () => {
        listeners.delete(callback);
      };
    }),
    listSummaries: vi.fn(async () =>
      projects.map((item) => ({
        ...summary,
        id: item.id,
        name: item.title,
        schedulerState: item.status,
      })),
    ),
    getSummary: vi.fn(async (id) => ({
      ...summary,
      id,
      schedulerState: projects.find((item) => item.id === id)?.status ?? "paused",
    })),
    getTree: vi.fn(async () => [rootNode]),
    getNode: vi.fn(async () => rootNode),
    getTaskViews: vi.fn(async () => []),
    getScheduler: vi.fn(async () => scheduler),
    startAutomatic: vi.fn(async () => {
      projects = [{ ...project, status: "running" }];
      tasks = [{ ...task, status: "running" }];
      results = [{ taskId: "t1", lines: [line] }];
    }),
    stop: vi.fn(async () => {}),
    manualProlong: vi.fn(async () => task),
    onProgress: vi.fn(async () => () => {}),
    listProjects: vi.fn(async () => projects),
    listTasks: vi.fn(async () => tasks),
    listResults: vi.fn(async () => results),
    createProject: vi.fn(async () => {
      projects = [project];
      tasks = [task];
      return project;
    }),
    queueRoot: vi.fn(async () => {
      tasks = [task];
      return task;
    }),
    start: vi.fn(async () => {
      projects = [{ ...project, status: "running" }];
      tasks = [{ ...task, status: "running" }];
      results = [{ taskId: "t1", lines: [line] }];
    }),
    pauseAfterCurrent: vi.fn(async () => {
      projects = [{ ...project, status: "paused" }];
      tasks = [{ ...task, status: "queued" }];
    }),
    pause: vi.fn(async () => {
      projects = [{ ...project, status: "paused" }];
      tasks = [{ ...task, status: "queued" }];
    }),
    cancel: vi.fn(async () => {
      projects = [{ ...project, status: "cancelled" }];
      tasks = [{ ...task, status: "cancelled" }];
    }),
    resume: vi.fn(async () => {
      projects = [{ ...project, status: "running" }];
      tasks = [{ ...task, status: "running" }];
    }),
  };
  return service;
}

describe("Deep Analysis desktop workflow", () => {
  it("subscribes once, refreshes read models on an update, and unsubscribes on unmount", async () => {
    // The 15-second fallback cannot fire here: only the backend event may refresh.
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    const service = fixture(true);
    await render(service);
    expect(service.updateListenerCount()).toBe(1);
    expect(service.onUpdate).toHaveBeenCalledTimes(1);
    expect(service.getTree).toHaveBeenCalledTimes(1);
    const nextNode: AnalysisNodeView = {
      ...rootNode,
      id: "2",
      parentId: "1",
      moveUci: "e2e4",
      moveSan: "e4",
      status: "unanalysed",
    };
    vi.mocked(service.getTree).mockResolvedValue([rootNode, nextNode]);
    await act(async () =>
      service.emitUpdate({ projectId: "p1", kind: "tree_node_updated", taskId: "t1" }),
    );
    expect(host.textContent).toContain("e4");
    expect(service.getTree).toHaveBeenCalledTimes(2);
    const view = root!;
    await act(async () =>
      view.render(
        <MantineProvider>
          <DeepAnalysisPage
            service={service}
            installedEngines={[{ name: "Stockfish", path: "/usr/bin/stockfish" }]}
          />
        </MantineProvider>,
      ),
    );
    expect(service.updateListenerCount()).toBe(1);
    expect(service.onUpdate).toHaveBeenCalledTimes(1);
    const late = service.latestUpdateListener()!;
    await act(async () => view.unmount());
    root = undefined;
    expect(service.updateListenerCount()).toBe(0);
    const reads = vi.mocked(service.getTree).mock.calls.length;
    await act(async () => {
      service.emitUpdate({ projectId: "p1", kind: "tree_node_updated", taskId: "t1" });
      late({ projectId: "p1", kind: "tree_node_updated", taskId: "t1" });
    });
    expect(service.getTree).toHaveBeenCalledTimes(reads);
    vi.useRealTimers();
  });

  it("refreshes scheduler stage and project controls from semantic updates", async () => {
    const service = fixture(true);
    await render(service);
    vi.mocked(service.listProjects).mockResolvedValue([{ ...project, status: "running" }]);
    vi.mocked(service.getScheduler).mockResolvedValue({
      ...scheduler,
      state: "running",
      stage: "alternatives",
      queueLength: 2,
    });
    await act(async () =>
      service.emitUpdate({ projectId: "p1", kind: "scheduler_stage_changed", taskId: null }),
    );
    expect(host.textContent).toContain("Stage alternatives");
    expect(host.textContent).toContain("Queued 2");
    expect(host.textContent).toContain("Pause analysis");
  });

  it("shows a selectable analysis tree and typed evaluation and queues manual prolong on a child", async () => {
    const service = fixture(true);
    const child: AnalysisNodeView = {
      ...rootNode,
      id: "2",
      parentId: "1",
      moveUci: "e2e4",
      moveSan: "e4",
      fen: "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1",
      sideToMove: "black",
      rawEvaluation: { kind: "centipawns", value: 37, display: "+0.37" },
      minimaxEvaluation: { kind: "centipawns", value: 37, display: "+0.37" },
      treeIndex: 55,
      depth: 18,
      pv: ["e7e5"],
      status: "unanalysed",
    };
    vi.mocked(service.getTree).mockResolvedValue([rootNode, child]);
    await render(service);
    expect(host.textContent).toContain("Analysis tree");
    expect(host.textContent).toContain("Scheduler");
    await click("e4");
    expect(host.textContent).toContain("Tree index 55");
    expect(host.textContent).toContain(child.fen);
    await click("Prolong selected node");
    expect(service.manualProlong).toHaveBeenCalledWith("p1", "2", 1);
  });

  it("renders the selected occurrence on the existing board when a pawn moves from e2 to e4", async () => {
    const service = fixture(true);
    const child: AnalysisNodeView = {
      ...rootNode,
      id: "2",
      parentId: "1",
      moveUci: "e2e4",
      moveSan: "e4",
      fen: AFTER_E4_FEN,
      sideToMove: "black",
      status: "unanalysed",
    };
    vi.mocked(service.getTree).mockResolvedValue([rootNode, child]);
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue(
      new DOMRect(0, 0, 800, 800),
    );
    // Finish Chessground's animation frame deterministically in jsdom.
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      callback(performance.now() + 1000);
      return 1;
    });
    await render(service);

    const selectedFen = () =>
      [...host.querySelectorAll("h5")].find((heading) => heading.textContent === "Selected node")
        ?.nextElementSibling?.textContent;
    const board = host.querySelector("cg-board");
    expect(board).toBeTruthy();
    const pawnPositions = () =>
      [...board!.querySelectorAll("piece.white.pawn")]
        .map((piece) => piece.getAttribute("style"))
        .sort();

    expect(selectedFen()).toBe(FEN);
    const before = pawnPositions();
    expect(before).toHaveLength(8);
    expect(before).toContain("transform: translate(400px,600px);"); // e2
    await click("e4");
    expect(selectedFen()).toBe(AFTER_E4_FEN);
    expect(host.querySelector("cg-board")).toBe(board);
    const after = pawnPositions();
    expect(after).toHaveLength(8);
    expect(after).toContain("transform: translate(400px,400px);"); // e4
    expect(after).not.toContain("transform: translate(400px,600px);");
    expect(after.filter((position) => !before.includes(position))).toHaveLength(1);
    expect(before.filter((position) => !after.includes(position))).toHaveLength(1);
  });

  it("marks both paths to a shared position and keeps occurrence selection on the same board", async () => {
    const service = fixture(true);
    const finalFen = "r1bqkb1r/pppppppp/2n2n2/8/8/2N2N2/PPPPPPPP/R1BQKB1R w KQkq - 4 3";
    const first: AnalysisNodeView = {
      ...rootNode,
      id: "8",
      parentId: "7",
      positionId: "42",
      transpositionCount: 2,
      transpositionLinked: true,
      moveUci: "b8c6",
      moveSan: "Nc6",
      fen: finalFen,
    };
    const second: AnalysisNodeView = {
      ...first,
      id: "9",
      parentId: "6",
      moveUci: "g8f6",
      moveSan: "Nf6",
    };
    vi.mocked(service.getTree).mockResolvedValue([rootNode, first, second]);
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue(
      new DOMRect(0, 0, 800, 800),
    );
    vi.spyOn(window, "requestAnimationFrame").mockImplementation((callback) => {
      callback(performance.now() + 1000);
      return 1;
    });
    await render(service);
    const tree = host.querySelector('[role="tree"]')!;
    expect(tree.textContent).not.toContain("Transposition · 1 paths");
    expect(tree.textContent?.match(/Transposition · 2 paths/g)).toHaveLength(2);
    const selectedFen = () =>
      [...host.querySelectorAll("h5")].find((heading) => heading.textContent === "Selected node")
        ?.nextElementSibling?.textContent;
    const board = host.querySelector("cg-board")!;
    for (const move of ["Nc6", "Nf6"]) {
      await click(move);
      expect(selectedFen()).toBe(finalFen);
      expect(tree.querySelector(`[aria-selected="true"]`)?.textContent).toBe(move);
      expect(
        [...board.querySelectorAll("piece.white.knight")].map((piece) =>
          piece.getAttribute("style"),
        ),
      ).toEqual(
        expect.arrayContaining([
          "transform: translate(200px,500px);",
          "transform: translate(500px,500px);",
        ]),
      );
    }
  });

  it("refreshes shared evaluation on both transposed paths while keeping path minimax separate", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    const service = fixture(true);
    const first: AnalysisNodeView = {
      ...rootNode,
      id: "8",
      parentId: "7",
      positionId: "42",
      transpositionCount: 2,
      transpositionLinked: true,
      moveUci: "b8c6",
      moveSan: "Nc6",
      status: "unanalysed",
    };
    const second: AnalysisNodeView = {
      ...first,
      id: "9",
      parentId: "6",
      moveUci: "g8f6",
      moveSan: "Nf6",
    };
    vi.mocked(service.getTree).mockResolvedValue([rootNode, first, second]);
    await render(service);
    await click("Nc6");
    expect(host.textContent).toContain("Raw evaluation Unknown");
    const updated = { kind: "centipawns", value: 28, display: "+0.28" } as const;
    vi.mocked(service.getTree).mockResolvedValue([
      rootNode,
      {
        ...first,
        rawEvaluation: updated,
        treeIndex: 52,
        minimaxEvaluation: updated,
        status: "analysed",
      },
      {
        ...second,
        rawEvaluation: updated,
        treeIndex: 52,
        minimaxEvaluation: { kind: "centipawns", value: 16, display: "+0.16" },
        status: "analysed",
      },
    ]);
    await act(async () =>
      service.emitUpdate({ projectId: "p1", kind: "tree_node_updated", taskId: "t1" }),
    );
    const tree = host.querySelector('[role="tree"]')!;
    expect(tree.textContent?.match(/#52/g)).toHaveLength(2);
    expect(host.textContent).toContain("Raw evaluation +0.28");
    expect(host.textContent).toContain("Minimax +0.28");
    await click("Nf6");
    expect(host.textContent).toContain("Raw evaluation +0.28");
    expect(host.textContent).toContain("Minimax +0.16");
    expect(service.getTree).toHaveBeenCalledTimes(2);
    vi.useRealTimers();
  });

  it("starts automatic analysis from the persisted project and stops while retaining work", async () => {
    const service = fixture(true);
    await render(service);
    await click("Start analysis");
    expect(service.startAutomatic).toHaveBeenCalledWith("p1");
    await click("Stop analysis");
    expect(service.stop).toHaveBeenCalledWith("p1", false);
  });

  it("refreshes the tree when progress is emitted instead of waiting for a fast poll", async () => {
    const service = fixture(true);
    let callback!: (progress: DeepAnalysisProgress) => void;
    vi.mocked(service.onProgress).mockImplementation(async (fn) => {
      callback = fn;
      return () => {};
    });
    await render(service);
    const before = vi.mocked(service.getTree).mock.calls.length;
    await act(async () => callback({ projectId: "p1" } as never));
    expect(vi.mocked(service.getTree).mock.calls.length).toBeGreaterThan(before);
  });

  it("restores persisted projects, task statuses and principal variations on reopen", async () => {
    const service = fixture(true);
    await render(service);
    expect(host.textContent).toContain("Opening study");
    expect(host.textContent).toContain(FEN);
    expect(host.textContent).toContain("Depth 18");
    expect(host.textContent).toContain("e2e4 e7e5");
  });

  it("polls persisted running task status and latest search depth and PV", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    try {
      const service = fixture(true);
      await render(service);
      vi.mocked(service.listProjects).mockResolvedValue([{ ...project, status: "running" }]);
      vi.mocked(service.listTasks).mockResolvedValue([{ ...task, status: "running" }]);
      vi.mocked(service.listResults).mockResolvedValue([
        { taskId: task.id, lines: [{ ...line, depth: 24, uciMoves: ["d2d4", "d7d5"] }] },
      ]);
      await act(async () => vi.advanceTimersByTimeAsync(15000));
      expect(host.textContent).toContain("Depth 24");
      expect(host.textContent).toContain("d2d4 d7d5");
      expect(host.textContent).not.toContain("e2e4 e7e5");
      expect(host.textContent).toContain("running");
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps a late poll for the old project from replacing the newly selected project", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    try {
      const service = fixture(true);
      const otherProject = { ...project, id: "p2", title: "Endgame study" };
      const otherTask = { ...task, id: "t2", projectId: "p2", fen: "8/8/8/8/8/8/8/8 w - - 0 1" };
      vi.mocked(service.listProjects).mockResolvedValue([project, otherProject]);
      let resolveOldTasks!: (value: (typeof task)[]) => void;
      vi.mocked(service.listTasks).mockImplementation((id) => {
        if (id === "p2") return Promise.resolve([otherTask]);
        if (resolveOldTasks)
          return new Promise((resolve) => {
            resolveOldTasks = resolve;
          });
        // Initial load is immediate; the following poll is held until after selection changes.
        resolveOldTasks = () => {};
        return Promise.resolve([task]);
      });
      vi.mocked(service.listResults).mockImplementation(async (id) =>
        id === "p2" ? [] : [{ taskId: task.id, lines: [line] }],
      );
      await render(service);
      await act(async () => vi.advanceTimersByTimeAsync(15000));
      const selection = host.querySelector<HTMLInputElement>('[aria-label="Project"]')!;
      await act(async () => selection.click());
      const option = [...document.querySelectorAll('[role="option"]')].find((node) =>
        node.textContent?.includes("Endgame study"),
      ) as HTMLElement;
      expect(option).toBeTruthy();
      await act(async () => option.click());
      expect(selection.value).toBe("Endgame study");
      expect(host.textContent).toContain(otherTask.fen);
      await act(async () => resolveOldTasks([task]));
      expect(selection.value).toBe("Endgame study");
      expect(host.textContent).toContain(otherTask.fen);
      expect(host.textContent).not.toContain("Depth 18");
    } finally {
      vi.useRealTimers();
    }
  });

  it("clears the old project's PV immediately while the selected project's load is pending", async () => {
    const service = fixture(true);
    const otherProject = { ...project, id: "p2", title: "Endgame study" };
    vi.mocked(service.listProjects).mockResolvedValue([project, otherProject]);
    let resolveNew!: (value: (typeof task)[]) => void;
    vi.mocked(service.listTasks).mockImplementation((id) =>
      id === "p2"
        ? new Promise((resolve) => {
            resolveNew = resolve;
          })
        : Promise.resolve([task]),
    );
    await render(service);
    const selection = host.querySelector<HTMLInputElement>('[aria-label="Project"]')!;
    await act(async () => selection.click());
    const option = [...document.querySelectorAll('[role="option"]')].find((node) =>
      node.textContent?.includes("Endgame study"),
    ) as HTMLElement;
    await act(async () => option.click());
    expect(selection.value).toBe("Endgame study");
    expect(host.textContent).not.toContain("Depth 18");
    await act(async () => resolveNew([]));
  });

  it("ignores a late polling error for a project after switching projects", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    try {
      const service = fixture(true);
      const otherProject = { ...project, id: "p2", title: "Endgame study" };
      vi.mocked(service.listProjects).mockResolvedValue([project, otherProject]);
      let rejectOld!: (reason: Error) => void;
      let oldCalls = 0;
      vi.mocked(service.listTasks).mockImplementation((id) => {
        if (id === "p2") return Promise.resolve([]);
        return ++oldCalls === 1
          ? Promise.resolve([task])
          : new Promise((_, reject) => {
              rejectOld = reject;
            });
      });
      await render(service);
      await act(async () => vi.advanceTimersByTimeAsync(15000));
      const selection = host.querySelector<HTMLInputElement>('[aria-label="Project"]')!;
      await act(async () => selection.click());
      const option = [...document.querySelectorAll('[role="option"]')].find((node) =>
        node.textContent?.includes("Endgame study"),
      ) as HTMLElement;
      await act(async () => option.click());
      expect(selection.value).toBe("Endgame study");
      await act(async () => rejectOld(new Error("stale poll failed")));
      expect(host.textContent).not.toContain("stale poll failed");
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not dispatch a second start while the first is pending and keeps command errors visible", async () => {
    const service = fixture(true);
    let rejectStart!: (reason: Error) => void;
    vi.mocked(service.startAutomatic).mockImplementation(
      () =>
        new Promise<void>((_, reject) => {
          rejectStart = reject;
        }),
    );
    await render(service);
    await click("Start analysis");
    await click("Start analysis");
    expect(service.startAutomatic).toHaveBeenCalledTimes(1);
    await act(async () => rejectStart(new Error("engine process unavailable")));
    expect(host.textContent).toContain("engine process unavailable");
    expect(host.textContent).toContain("Opening study");
  });

  it("cancels a running project and removes controls that would restart it", async () => {
    const service = fixture(true);
    vi.mocked(service.listProjects).mockResolvedValueOnce([{ ...project, status: "running" }]);
    await render(service);
    await click("Cancel project");
    expect(service.cancel).toHaveBeenCalledWith("p1");
    expect(host.textContent).toContain("cancelled");
    const start = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Start analysis",
    );
    expect(start?.disabled).toBe(true);
    await change(
      "Root position to queue",
      "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1",
    );
    const queue = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Queue root position",
    );
    expect(queue?.disabled).toBe(true);
    expect(host.textContent).not.toContain("Cancel project");
  });

  it("does not restart completed positions when reopening a finished project", async () => {
    const service = fixture(true);
    vi.mocked(service.listProjects).mockResolvedValue([{ ...project, status: "completed" }]);
    vi.mocked(service.listTasks).mockResolvedValue([{ ...task, status: "completed" }]);
    await render(service);
    const start = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Start analysis",
    );
    expect(start?.disabled).toBe(true);
    await click("Start analysis");
    expect(service.startAutomatic).not.toHaveBeenCalled();
  });

  it("shows persisted per-task execution metadata when an event queues priority work", async () => {
    const service = fixture(true);
    await render(service);
    const queued: AnalysisTaskView = {
      id: "t-priority",
      strategy: "Prolong",
      priority: 1,
      sourceNodeId: "147",
      positionFen: FEN,
      state: "queued",
      requestedTimeMs: 900,
      requestedDepth: 19,
      operationMode: "time_or_depth",
      remainingProlongCount: 2,
      engine: "/usr/bin/stockfish",
      engineProfile: "default",
      requestedMultipv: 2,
      cachePolicy: "compatible_only",
      createdAt: null,
    };
    vi.mocked(service.getTaskViews).mockResolvedValue([queued]);
    await act(async () =>
      service.emitUpdate({ projectId: "p1", kind: "task_queued", taskId: queued.id }),
    );
    expect(host.textContent).toContain("Prolong · queued · Priority 1");
    expect(host.textContent).toContain("900 ms · depth 19 · time_or_depth");
    expect(host.textContent).toContain("Source 147 · 2 prolongs left");
    expect(host.textContent).toContain("Profile default · MultiPV 2 · Cache compatible_only");
  });

  it("offers immediate pause separately and blocks a second scheduler during finish-current pause", async () => {
    const service = fixture(true);
    vi.mocked(service.listProjects).mockResolvedValueOnce([{ ...project, status: "running" }]);
    await render(service);
    await click("Pause immediately");
    expect(service.pause).toHaveBeenCalledWith("p1");
    expect(service.pauseAfterCurrent).not.toHaveBeenCalled();

    vi.mocked(service.listProjects).mockResolvedValue([{ ...project, status: "pausing" }]);
    await act(async () =>
      service.emitUpdate({ projectId: "p1", kind: "project_state_changed", taskId: null }),
    );
    const start = [...host.querySelectorAll("button")].find(
      (button) => button.textContent?.trim() === "Start analysis",
    );
    expect(start?.disabled).toBe(true);
    expect(host.textContent).not.toContain("Pause immediately");
    expect(host.textContent).not.toContain("Resume analysis");
  });

  it("creates a named project with time/MultiPV and its root already persisted, then controls analysis", async () => {
    const service = fixture();
    await render(service);
    await change("Project name", "Opening study");
    await change("Root FEN", FEN);
    await change("Time per position (seconds)", "10");
    await change("MultiPV", "2");
    await click("Create project");
    expect(service.createProject).toHaveBeenCalledWith(
      "Opening study",
      "/usr/bin/stockfish",
      10000,
      2,
      FEN,
      [],
    );
    expect(service.queueRoot).not.toHaveBeenCalled();
    expect(host.textContent).toContain(FEN);
    expect(host.textContent).toContain("10 seconds");
    await click("Start analysis");
    expect(service.startAutomatic).toHaveBeenCalledWith("p1");
    expect(host.textContent).toContain("Depth 18");
    expect(host.textContent).toContain("+0.37");
    expect(host.textContent).toContain("e2e4 e7e5");
    await click("Pause analysis");
    expect(service.pauseAfterCurrent).toHaveBeenCalledWith("p1");
    expect(service.pause).not.toHaveBeenCalled();
    await click("Resume analysis");
    expect(service.resume).toHaveBeenCalledWith("p1");
  });

  it("limits each position to 3600 seconds and rejects a larger time before calling the backend", async () => {
    const service = fixture();
    await render(service);
    await change("Project name", "Study");
    await change("Time per position (seconds)", "3601");
    await click("Create project");
    expect(service.createProject).not.toHaveBeenCalled();
    expect(host.textContent).toContain("between 1 and 3600 seconds");
  });

  it("accepts the 3600-second boundary and displays the maximum in the input", async () => {
    const service = fixture();
    await render(service);
    expect(host.textContent).toContain("Maximum 3600 seconds per position.");
    await change("Project name", "Study");
    await change("Time per position (seconds)", "3600");
    await click("Create project");
    expect(service.createProject).toHaveBeenCalledWith(
      "Study",
      "/usr/bin/stockfish",
      3600000,
      1,
      FEN,
      [],
    );
  });

  it("labels positive and negative evaluation scores as white's perspective", async () => {
    const service = fixture(true);
    vi.mocked(service.listResults).mockResolvedValue([
      {
        taskId: task.id,
        lines: [line, { ...line, multipv: 2, scoreKind: "mate", scoreValue: -3 }],
      },
    ]);
    await render(service);
    expect(host.textContent).toContain("+0.37 (White's perspective)");
    expect(host.textContent).toContain("Mate -3 (White's perspective)");
  });

  it("validates blank names and illegal FEN before project creation or queueing", async () => {
    const service = fixture(true);
    await render(service);
    await click("Create project");
    expect(host.textContent).toContain("Enter a project name.");
    expect(service.createProject).not.toHaveBeenCalled();
    await change("Root position to queue", "8/8/8/8/8/8/8/8 w - - 0 1");
    await click("Queue root position");
    expect(host.textContent).toContain("Enter a valid FEN position.");
    expect(service.queueRoot).not.toHaveBeenCalled();
  });

  it("accepts a local engine installed after the screen opens", async () => {
    const service = fixture();
    await render(service, []);
    await act(async () =>
      root!.render(
        <MantineProvider>
          <DeepAnalysisPage
            service={service}
            installedEngines={[{ name: "Stockfish", path: "/usr/bin/stockfish" }]}
          />
        </MantineProvider>,
      ),
    );
    await change("Project name", "Study");
    await click("Create project");
    expect(service.createProject).toHaveBeenCalledWith(
      "Study",
      "/usr/bin/stockfish",
      10000,
      1,
      FEN,
      [],
    );
  });

  it("rejects invalid FEN before queueing and explains missing installed engines", async () => {
    const service = fixture();
    await render(service, []);
    await change("Project name", "Study");
    await change("Root FEN", "bad fen");
    await click("Create project");
    expect(host.textContent).toContain("Enter a valid FEN position.");
    expect(host.textContent).toContain("Install a local engine");
    expect(service.createProject).not.toHaveBeenCalled();
  });
});
