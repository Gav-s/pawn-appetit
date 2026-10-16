import {
  Alert,
  Anchor,
  Badge,
  Button,
  Group,
  NumberInput,
  Paper,
  Select,
  SimpleGrid,
  Stack,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { INITIAL_FEN } from "chessops/fen";
import { useCallback, useEffect, useRef, useState } from "react";
import { Chessground } from "@/components/Chessground";
import type {
  AnalysisNodeView,
  AnalysisProjectSummary,
  AnalysisTaskView,
  SchedulerView,
  DeepAnalysisProject,
  DeepAnalysisResult,
  DeepAnalysisService,
  DeepAnalysisTask,
} from "./service";
import { validateDeepAnalysisProject } from "./validation";

export interface InstalledEngine {
  name: string;
  path: string;
}

function score(kind: string, value: number) {
  if (kind === "mate") return `Mate ${value > 0 ? "+" : ""}${value}`;
  return `${value >= 0 ? "+" : ""}${(value / 100).toFixed(2)}`;
}

export default function DeepAnalysisPage({
  service,
  installedEngines,
}: {
  service: DeepAnalysisService;
  installedEngines: InstalledEngine[];
}) {
  const [projects, setProjects] = useState<DeepAnalysisProject[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [tasks, setTasks] = useState<DeepAnalysisTask[]>([]);
  const [results, setResults] = useState<DeepAnalysisResult[]>([]);
  const [summaries, setSummaries] = useState<AnalysisProjectSummary[]>([]);
  const [tree, setTree] = useState<AnalysisNodeView[]>([]);
  const [taskViews, setTaskViews] = useState<AnalysisTaskView[]>([]);
  const [scheduler, setScheduler] = useState<SchedulerView | null>(null);
  const [selectedNodeId, setSelectedNodeId] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [fen, setFen] = useState(INITIAL_FEN);
  const [enginePath, setEnginePath] = useState<string | null>(installedEngines[0]?.path ?? null);
  useEffect(() => {
    if (!enginePath && installedEngines[0]) setEnginePath(installedEngines[0].path);
  }, [enginePath, installedEngines]);
  const [seconds, setSeconds] = useState<number | string>(10);
  const [multiPv, setMultiPv] = useState<number | string>(1);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const busyRef = useRef(false);
  const selectedIdRef = useRef<string | null>(null);
  const refreshVersion = useRef(0);

  const refresh = useCallback(
    async (projectId?: string | null) => {
      const version = ++refreshVersion.current;
      try {
        const [loaded, nextSummaries] = await Promise.all([
          service.listProjects(),
          service.listSummaries(),
        ]);
        if (version !== refreshVersion.current) return;
        setProjects(loaded);
        setSummaries(nextSummaries);
        const id = projectId ?? selectedIdRef.current ?? loaded[0]?.id;
        if (id) {
          const [nextTasks, nextResults, nextTree, nextScheduler, nextTaskViews] =
            await Promise.all([
              service.listTasks(id),
              service.listResults(id),
              service.getTree(id),
              service.getScheduler(id),
              service.getTaskViews(id),
            ]);
          if (version !== refreshVersion.current) return;
          selectedIdRef.current = id;
          setSelectedId(id);
          setTasks(nextTasks);
          setResults(nextResults);
          setTree(nextTree);
          setScheduler(nextScheduler);
          setTaskViews(nextTaskViews);
          setSelectedNodeId((prior) =>
            nextTree.some((node) => node.id === prior) ? prior : (nextTree[0]?.id ?? null),
          );
        } else {
          selectedIdRef.current = null;
          setSelectedId(null);
          setTasks([]);
          setResults([]);
          setTree([]);
          setTaskViews([]);
          setScheduler(null);
          setSelectedNodeId(null);
        }
      } catch (reason) {
        if (version === refreshVersion.current) throw reason;
      }
    },
    [service],
  );

  useEffect(() => {
    let active = true;
    const versionRef = refreshVersion;
    void refresh().catch((reason: unknown) => {
      if (active) setError(String(reason));
    });
    return () => {
      active = false;
      versionRef.current++;
    };
  }, [refresh]);

  useEffect(() => {
    let active = true;
    const detachers: Array<() => void> = [];
    const changed = (projectId: string) => {
      if (active && projectId === selectedIdRef.current) {
        void refresh(projectId).catch((reason: unknown) => {
          if (active) setError(String(reason));
        });
      }
    };
    const attach = (subscribe: () => Promise<() => void>) => {
      void Promise.resolve()
        .then(subscribe)
        .then((unlisten) => {
          if (active) detachers.push(unlisten);
          else unlisten();
        })
        .catch((reason: unknown) => {
          if (active) setError(String(reason));
        });
    };
    attach(() => service.onUpdate((update) => changed(update.projectId)));
    attach(() => service.onProgress((progress) => changed(progress.projectId)));
    return () => {
      active = false;
      detachers.forEach((unlisten) => unlisten());
    };
  }, [refresh, service]);

  useEffect(() => {
    if (!selectedId) return;
    const timer = window.setInterval(() => {
      void refresh(selectedId).catch((reason: unknown) => setError(String(reason)));
    }, 15000);
    return () => window.clearInterval(timer);
  }, [refresh, selectedId]);

  const project = projects.find((item) => item.id === selectedId);
  const summary = summaries.find((item) => item.id === selectedId);
  const selectedNode = tree.find((node) => node.id === selectedNodeId) ?? tree[0];
  const isRunning = project?.status === "running";
  const isPausing = project?.status === "pausing";
  const isPaused =
    project?.status === "paused" || project?.status === "stopped" || project?.status === "failed";
  const hasQueuedTasks = tasks.some((task) => task.status === "queued");

  async function perform(action: () => Promise<unknown>, projectId?: string) {
    if (busyRef.current) return;
    busyRef.current = true;
    setError("");
    setBusy(true);
    try {
      await action();
      await refresh(projectId);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      busyRef.current = false;
      setBusy(false);
    }
  }

  async function create() {
    const validation = validateDeepAnalysisProject(name, fen);
    if (validation) {
      setError(validation);
      return;
    }
    if (!enginePath || !installedEngines.some((engine) => engine.path === enginePath)) {
      setError("Select an installed local engine.");
      return;
    }
    const budget = Number(seconds);
    const lines = Number(multiPv);
    if (
      !Number.isInteger(budget) ||
      budget < 1 ||
      budget > 3600 ||
      !Number.isInteger(lines) ||
      lines < 1 ||
      lines > 32
    ) {
      setError("Time must be between 1 and 3600 seconds and MultiPV between 1 and 32.");
      return;
    }
    await perform(async () => {
      const created = await service.createProject(
        name.trim(),
        enginePath,
        budget * 1000,
        lines,
        fen.trim(),
        [],
      );
      setSelectedId(created.id);
      setName("");
      await refresh(created.id);
    });
  }

  return (
    <Stack p="lg" gap="lg" style={{ overflowY: "auto", flex: 1 }}>
      <div>
        <Title order={2}>Deep Analysis</Title>
        <Text c="dimmed">Persisted engine analysis for a position.</Text>
      </div>
      {error && (
        <Alert color="red" title="Deep Analysis error" role="alert">
          {error}
        </Alert>
      )}
      <Paper withBorder p="lg">
        <Stack>
          <Title order={3}>New project</Title>
          <TextInput
            label="Project name"
            aria-label="Project name"
            value={name}
            onChange={(event) => setName(event.currentTarget.value)}
          />
          <TextInput
            label="Root FEN"
            aria-label="Root FEN"
            value={fen}
            onChange={(event) => setFen(event.currentTarget.value)}
          />
          <Select
            label="Installed engine"
            aria-label="Installed engine"
            data={installedEngines.map((engine) => ({ label: engine.name, value: engine.path }))}
            value={enginePath}
            onChange={setEnginePath}
            searchable
            allowDeselect={false}
          />
          {installedEngines.length === 0 && (
            <Text c="dimmed">
              Install a local engine in <Anchor href="/engines">Engines</Anchor> first.
            </Text>
          )}
          <Group grow align="start">
            <NumberInput
              label="Time per position (seconds)"
              aria-label="Time per position (seconds)"
              min={1}
              max={3600}
              description="Maximum 3600 seconds per position."
              step={1}
              value={seconds}
              onChange={setSeconds}
            />
            <NumberInput
              label="MultiPV"
              aria-label="MultiPV"
              min={1}
              max={32}
              step={1}
              value={multiPv}
              onChange={setMultiPv}
            />
          </Group>
          <Button onClick={() => void create()} loading={busy}>
            Create project
          </Button>
        </Stack>
      </Paper>
      <Paper withBorder p="lg">
        <Stack>
          <Title order={3}>Projects</Title>
          {projects.length === 0 ? (
            <Text c="dimmed">No projects yet. Create one above.</Text>
          ) : (
            <Select
              label="Project"
              aria-label="Project"
              value={selectedId}
              data={projects.map((item) => ({ value: item.id, label: item.title }))}
              onChange={(value) => {
                if (!value || busyRef.current || value === selectedIdRef.current) return;
                refreshVersion.current++;
                selectedIdRef.current = value;
                setSelectedId(value);
                setTasks([]);
                setResults([]);
                setTree([]);
                setTaskViews([]);
                setScheduler(null);
                setSelectedNodeId(null);
                setError("");
                void refresh(value).catch((reason: unknown) => setError(String(reason)));
              }}
              disabled={busy}
              allowDeselect={false}
            />
          )}
          {project && (
            <>
              <Text size="sm" fw={500}>
                {project.title}
              </Text>
              <Group>
                <Badge>{project.status}</Badge>
                <Text size="sm">
                  {project.timeMs / 1000} seconds per position · MultiPV {project.multipv}
                </Text>
              </Group>
              <TextInput
                label="Root position to queue"
                aria-label="Root position to queue"
                value={fen}
                onChange={(event) => setFen(event.currentTarget.value)}
              />
              <Group>
                <Button
                  disabled={
                    busy ||
                    project.status === "cancelled" ||
                    tasks.some((task) => task.fen === fen.trim())
                  }
                  onClick={() => {
                    const validation = validateDeepAnalysisProject(project.title, fen);
                    if (validation) {
                      setError(validation);
                      return;
                    }
                    void perform(() => service.queueRoot(project.id, fen.trim()), project.id);
                  }}
                >
                  Queue root position
                </Button>
                <Button
                  disabled={
                    busy ||
                    isRunning ||
                    isPausing ||
                    !hasQueuedTasks ||
                    project.status === "cancelled"
                  }
                  onClick={() => void perform(() => service.startAutomatic(project.id), project.id)}
                >
                  Start analysis
                </Button>
                {isRunning && (
                  <>
                    <Button
                      disabled={busy}
                      variant="default"
                      onClick={() =>
                        void perform(() => service.pauseAfterCurrent(project.id), project.id)
                      }
                    >
                      Pause analysis
                    </Button>
                    <Button
                      disabled={busy}
                      variant="subtle"
                      onClick={() => void perform(() => service.pause(project.id), project.id)}
                    >
                      Pause immediately
                    </Button>
                  </>
                )}
                {isPaused && hasQueuedTasks && (
                  <Button
                    disabled={busy}
                    onClick={() => void perform(() => service.resume(project.id), project.id)}
                  >
                    Resume analysis
                  </Button>
                )}
                {project.status !== "cancelled" && project.status !== "stopped" && (
                  <Button
                    disabled={busy}
                    variant="default"
                    onClick={() => void perform(() => service.stop(project.id, false), project.id)}
                  >
                    Stop analysis
                  </Button>
                )}
                {project.status !== "cancelled" && (
                  <Button
                    disabled={busy}
                    variant="subtle"
                    color="red"
                    onClick={() => {
                      if (
                        window.confirm(
                          "Stop analysis and clear all queued tasks? Accepted analysis will be kept.",
                        )
                      ) {
                        void perform(() => service.stop(project.id, true), project.id);
                      }
                    }}
                  >
                    Stop & clear queued
                  </Button>
                )}
                {project.status !== "cancelled" && (
                  <Button
                    disabled={busy}
                    variant="default"
                    color="red"
                    onClick={() => void perform(() => service.cancel(project.id), project.id)}
                  >
                    Cancel project
                  </Button>
                )}
              </Group>
              <SimpleGrid cols={{ base: 1, lg: 3 }} spacing="md">
                <Paper withBorder p="sm">
                  <Title order={4}>Position</Title>
                  {selectedNode ? (
                    <div style={{ width: "100%", maxWidth: 420, aspectRatio: "1 / 1" }}>
                      <Chessground fen={selectedNode.fen} viewOnly />
                    </div>
                  ) : (
                    <Text c="dimmed">Position unavailable.</Text>
                  )}
                </Paper>
                <Paper withBorder p="sm">
                  <Title order={4}>Analysis tree</Title>
                  {tree.length === 0 && <Text c="dimmed">No tree nodes yet.</Text>}
                  <Stack gap={2} role="tree" aria-label="Analysis tree">
                    {tree.map((node) => (
                      <Group key={node.id} gap="xs" wrap="nowrap">
                        <Button
                          size="compact-sm"
                          variant={node.id === selectedNode?.id ? "filled" : "subtle"}
                          aria-selected={node.id === selectedNode?.id}
                          onClick={() => setSelectedNodeId(node.id)}
                        >
                          {node.moveSan ?? "Root"}
                        </Button>
                        <Text size="xs" c="dimmed">
                          {node.status}
                        </Text>
                        {node.treeIndex !== null && <Text size="xs">#{node.treeIndex}</Text>}
                        {node.transpositionCount > 1 && (
                          <Badge size="sm" variant="light">
                            Transposition · {node.transpositionCount} paths
                          </Badge>
                        )}
                      </Group>
                    ))}
                  </Stack>
                  {selectedNode && (
                    <Stack gap="xs" mt="md">
                      <Title order={5}>Selected node</Title>
                      <Text size="sm" style={{ overflowWrap: "anywhere" }}>
                        {selectedNode.fen}
                      </Text>
                      <Text size="sm">
                        Raw evaluation{" "}
                        {selectedNode.rawEvaluation.kind === "unknown"
                          ? "Unknown"
                          : selectedNode.rawEvaluation.display}
                      </Text>
                      <Text size="sm">
                        Minimax{" "}
                        {selectedNode.minimaxEvaluation.kind === "unknown"
                          ? "Unknown"
                          : selectedNode.minimaxEvaluation.display}
                      </Text>
                      <Text size="sm">Tree index {selectedNode.treeIndex ?? "—"}</Text>
                      <Text size="sm">
                        Depth {selectedNode.depth ?? "—"} · Time {selectedNode.elapsedMs ?? "—"} ms
                      </Text>
                      <Text size="sm">PV {selectedNode.pv.join(" ") || "pending"}</Text>
                      <Text size="xs" c="dimmed">
                        {selectedNode.sideToMove} to move · {selectedNode.engine ?? "No engine"}
                      </Text>
                      <Button
                        disabled={
                          busy ||
                          selectedNode.status !== "unanalysed" ||
                          project.status === "cancelled"
                        }
                        onClick={() =>
                          void perform(
                            () => service.manualProlong(project.id, selectedNode.id, 1),
                            project.id,
                          )
                        }
                      >
                        Prolong selected node
                      </Button>
                    </Stack>
                  )}
                </Paper>
                <Paper withBorder p="sm">
                  <Title order={4}>Scheduler</Title>
                  <Text size="sm">
                    {summary?.name ?? project.title} · {scheduler?.state ?? project.status}
                  </Text>
                  <Text size="sm">
                    Stage {scheduler?.stage ?? "—"} · Cycle {scheduler?.cycle ?? "—"}
                  </Text>
                  <Text size="sm">
                    Queued {scheduler?.queueLength ?? 0} · Completed{" "}
                    {scheduler?.completedCount ?? 0}
                  </Text>
                  <Text size="sm">Engine {scheduler?.engineState ?? "idle"}</Text>
                  <Text size="xs" c="dimmed" style={{ overflowWrap: "anywhere" }}>
                    {summary?.engine ?? project.engine}
                  </Text>
                  {scheduler?.exhausted && <Badge>Exhausted</Badge>}
                  <Title order={5}>Task queue</Title>
                  {taskViews
                    .filter((item) => item.state === "queued" || item.state === "running")
                    .map((item) => (
                      <Stack key={item.id} gap={0}>
                        <Text size="sm">
                          {item.strategy} · {item.state} · Priority {item.priority}
                        </Text>
                        <Text size="xs" c="dimmed">
                          {item.requestedTimeMs ?? "—"} ms · depth {item.requestedDepth ?? "—"} ·{" "}
                          {item.operationMode ?? "unknown"}
                        </Text>
                        <Text size="xs" c="dimmed">
                          Source {item.sourceNodeId ?? "root"} · {item.remainingProlongCount}{" "}
                          prolongs left
                        </Text>
                        <Text size="xs" c="dimmed">
                          Profile {item.engineProfile} · MultiPV {item.requestedMultipv} · Cache{" "}
                          {item.cachePolicy}
                        </Text>
                        <Text size="xs" c="dimmed" style={{ overflowWrap: "anywhere" }}>
                          {item.engine}
                        </Text>
                      </Stack>
                    ))}
                  {taskViews.length === 0 && (
                    <Text c="dimmed" size="sm">
                      No queued tasks.
                    </Text>
                  )}
                </Paper>
              </SimpleGrid>
              <Title order={4}>Positions</Title>
              {tasks.length === 0 ? (
                <Text c="dimmed">Queue the root position to begin.</Text>
              ) : (
                tasks.map((task) => (
                  <Paper key={task.id} withBorder p="sm">
                    <Stack gap="xs">
                      <Group>
                        <Badge variant="light">{task.status}</Badge>
                        <Text size="xs" c="dimmed">
                          Ply {task.ply}
                        </Text>
                      </Group>
                      <Text size="xs" c="dimmed" style={{ overflowWrap: "anywhere" }}>
                        {task.fen}
                      </Text>
                      {task.error && (
                        <Text c="red" size="sm">
                          {task.error}
                        </Text>
                      )}
                      {(results.find((result) => result.taskId === task.id)?.lines ?? []).map(
                        (line) => (
                          <Group key={line.multipv} gap="sm" wrap="wrap">
                            <Text size="sm">Line {line.multipv}</Text>
                            <Text size="sm">Depth {line.depth}</Text>
                            <Text size="sm">
                              {score(line.scoreKind, line.scoreValue)} (White's perspective)
                            </Text>
                            <Text size="sm" ff="monospace">
                              {line.uciMoves.join(" ")}
                            </Text>
                          </Group>
                        ),
                      )}
                      {!results.some((result) => result.taskId === task.id) && (
                        <Text size="sm" c="dimmed">
                          PV pending
                        </Text>
                      )}
                    </Stack>
                  </Paper>
                ))
              )}
            </>
          )}
        </Stack>
      </Paper>
    </Stack>
  );
}
