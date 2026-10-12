import { MantineProvider } from "@mantine/core";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";
import DeepAnalysisPage from "../DeepAnalysisPage";
import type { DeepAnalysisService } from "../service";

const FEN = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
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
let root: Root;
let host: HTMLDivElement;
afterEach(async () => {
  if (root) await act(async () => root.unmount());
  host?.remove();
});

async function render(
  service: DeepAnalysisService,
  engines = [{ name: "Stockfish", path: "/usr/bin/stockfish" }],
) {
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  await act(async () =>
    root.render(
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
  const service: DeepAnalysisService = {
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
      await act(async () => vi.advanceTimersByTimeAsync(1500));
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
      await act(async () => vi.advanceTimersByTimeAsync(1500));
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
      await act(async () => vi.advanceTimersByTimeAsync(1500));
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
    vi.mocked(service.start).mockImplementation(
      () =>
        new Promise<void>((_, reject) => {
          rejectStart = reject;
        }),
    );
    await render(service);
    await click("Start analysis");
    await click("Start analysis");
    expect(service.start).toHaveBeenCalledTimes(1);
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
    expect(service.start).not.toHaveBeenCalled();
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
    expect(service.start).toHaveBeenCalledWith("p1");
    expect(host.textContent).toContain("Depth 18");
    expect(host.textContent).toContain("+0.37");
    expect(host.textContent).toContain("e2e4 e7e5");
    await click("Pause analysis");
    expect(service.pause).toHaveBeenCalledWith("p1");
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
      root.render(
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
