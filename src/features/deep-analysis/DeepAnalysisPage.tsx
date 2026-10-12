import {
  Alert,
  Anchor,
  Badge,
  Button,
  Group,
  NumberInput,
  Paper,
  Select,
  Stack,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { INITIAL_FEN } from "chessops/fen";
import { useCallback, useEffect, useRef, useState } from "react";
import type {
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
        const loaded = await service.listProjects();
        if (version !== refreshVersion.current) return;
        setProjects(loaded);
        const id = projectId ?? selectedIdRef.current ?? loaded[0]?.id;
        if (id) {
          const [nextTasks, nextResults] = await Promise.all([
            service.listTasks(id),
            service.listResults(id),
          ]);
          if (version !== refreshVersion.current) return;
          selectedIdRef.current = id;
          setSelectedId(id);
          setTasks(nextTasks);
          setResults(nextResults);
        } else {
          selectedIdRef.current = null;
          setSelectedId(null);
          setTasks([]);
          setResults([]);
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
    if (!selectedId) return;
    const timer = window.setInterval(() => {
      void refresh(selectedId).catch((reason: unknown) => setError(String(reason)));
    }, 1500);
    return () => window.clearInterval(timer);
  }, [refresh, selectedId]);

  const project = projects.find((item) => item.id === selectedId);
  const isRunning = project?.status === "running";
  const isPaused = project?.status === "paused";
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
                  disabled={busy || isRunning || !hasQueuedTasks || project.status === "cancelled"}
                  onClick={() => void perform(() => service.start(project.id), project.id)}
                >
                  Start analysis
                </Button>
                {isRunning && (
                  <Button
                    disabled={busy}
                    variant="default"
                    onClick={() => void perform(() => service.pause(project.id), project.id)}
                  >
                    Pause analysis
                  </Button>
                )}
                {isPaused && hasQueuedTasks && (
                  <Button
                    disabled={busy}
                    onClick={() => void perform(() => service.resume(project.id), project.id)}
                  >
                    Resume analysis
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
