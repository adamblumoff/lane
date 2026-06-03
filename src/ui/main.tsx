import { StrictMode, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Alert,
  AppShell,
  Badge,
  Button,
  createTheme,
  Divider,
  Group,
  MantineProvider,
  NavLink,
  ScrollArea,
  Stack,
  Tabs,
  Text,
  TextInput,
  Textarea,
  Title,
} from "@mantine/core";
import "@mantine/core/styles.css";
import "./styles.css";

const DEFAULT_LANE = "agent-a";
const DEFAULT_PATH = "demo/example.ts";
const DEFAULT_NEW_LANE = "agent-c";

const theme = createTheme({
  primaryColor: "gray",
  primaryShade: { light: 7, dark: 4 },
  defaultRadius: "sm",
  fontFamily:
    'Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
});

type LaneView = {
  id: string;
  content: string;
  byte_len: number;
};

type FileView = {
  path: string;
  base: LaneView;
  lanes: LaneView[];
};

type LaneState = {
  storage_path: string;
  files: FileView[];
};

type AppView = {
  files: FileView[];
  lanes: LaneView[];
  activeFile: FileView | null;
  activeView: LaneView | null;
  canEdit: boolean;
  changedFiles: FileView[];
  changedPaths: Set<string>;
  changedCountByLane: Map<string, number>;
};

function App() {
  const [state, setState] = useState<LaneState | null>(null);
  const [activeLane, setActiveLane] = useState(DEFAULT_LANE);
  const [activePath, setActivePath] = useState(DEFAULT_PATH);
  const [buffer, setBuffer] = useState("");
  const [newLane, setNewLane] = useState(DEFAULT_NEW_LANE);
  const [error, setError] = useState("");
  const syncedContent = useRef("");

  useEffect(() => {
    void refresh();
  }, []);

  const view = useMemo(
    () => deriveAppView(state, activePath, activeLane, buffer),
    [activeLane, activePath, buffer, state],
  );

  useEffect(() => {
    if (!view.activeView) {
      return;
    }
    syncedContent.current = view.activeView.content;
    setBuffer(view.activeView.content);
  }, [activeLane, view.activeView]);

  useEffect(() => {
    if (!view.canEdit || !view.activeFile || buffer === syncedContent.current) {
      return;
    }

    const path = view.activeFile.path;
    const timeout = window.setTimeout(() => {
      void saveLane(activeLane, path, buffer);
    }, 450);

    return () => window.clearTimeout(timeout);
  }, [activeLane, buffer, view.activeFile, view.canEdit]);

  async function refresh() {
    await runRequest(() => apiJson<LaneState>("/api/state"));
  }

  async function saveLane(lane: string, path: string, content: string) {
    const saved = await runRequest(
      () =>
        apiJson<LaneState>(lanePath(lane, "/replace"), {
          path,
          content,
        }),
    );
    if (saved) {
      syncedContent.current = content;
    }
    return saved;
  }

  async function saveActiveEdit() {
    if (!view.canEdit || !view.activeFile || buffer === syncedContent.current) {
      return true;
    }
    return saveLane(activeLane, view.activeFile.path, buffer);
  }

  async function createLane() {
    const lane = newLane.trim();
    if (!lane || lane === "base") {
      setError("Use a lane id other than base.");
      return;
    }
    if (!(await saveActiveEdit())) {
      return;
    }
    if (view.lanes.some((existingLane) => existingLane.id === lane)) {
      setActiveLane(lane);
      return;
    }

    if (await runRequest(() => apiJson<LaneState>(lanePath(lane), {}))) {
      setActiveLane(lane);
    }
  }

  async function promoteLane() {
    if (!(await saveActiveEdit())) {
      return;
    }
    const promoted = await runRequest(() =>
      apiJson<LaneState>(lanePath(activeLane, "/promote"), {}),
    );
    if (promoted) {
      setActiveLane("base");
    }
  }

  async function promoteFile() {
    if (!view.activeFile) {
      return;
    }
    const path = view.activeFile.path;
    if (!(await saveActiveEdit())) {
      return;
    }
    const promoted = await runRequest(
      () =>
        apiJson<LaneState>(lanePath(activeLane, "/promote-file"), {
          path,
        }),
    );
    if (promoted) {
      setActiveLane("base");
    }
  }

  async function selectLane(lane: string) {
    if (lane === activeLane) {
      return;
    }
    if (!(await saveActiveEdit())) {
      return;
    }
    setActiveLane(lane);
  }

  async function selectFile(path: string) {
    if (path === activePath) {
      return;
    }
    if (!(await saveActiveEdit())) {
      return;
    }
    setActivePath(path);
  }

  async function reset() {
    if (await runRequest(() => apiJson<LaneState>("/api/reset", {}))) {
      setActiveLane(DEFAULT_LANE);
      setActivePath(DEFAULT_PATH);
    }
  }

  async function runRequest(request: () => Promise<LaneState>) {
    try {
      const nextState = await request();
      setState(nextState);
      setError("");
      return true;
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "Request failed");
      return false;
    }
  }

  const promoteLaneLabel = `Promote lane (${view.changedFiles.length} ${
    view.changedFiles.length === 1 ? "file" : "files"
  })`;
  const activeFileChanged = Boolean(
    view.activeFile && view.changedPaths.has(view.activeFile.path),
  );

  return (
    <AppShell
      navbar={{ width: 280, breakpoint: "sm" }}
      padding="md"
    >
      <AppShell.Navbar p="md">
        <Stack h="100%" gap="md">
          <div>
            <Title order={1} size="h2">
              Lane
            </Title>
            <Text c="dimmed" size="sm">
              {state?.storage_path ?? ".lane/repo.lane"}
            </Text>
          </div>

          {error ? (
            <Alert color="red" variant="light">
              {error}
            </Alert>
          ) : null}

          <Divider />

          <Stack gap={6}>
            <Text c="dimmed" fw={700} size="xs">
              LANES
            </Text>
            <ScrollArea.Autosize mah="calc(100vh - 270px)">
              <Stack gap={4}>
                {view.lanes.map((lane) => {
                  const changedCount = view.changedCountByLane.get(lane.id) ?? 0;
                  return (
                    <NavLink
                      active={lane.id === activeLane}
                      description={
                        lane.id === "base" ? `${view.files.length} files` : `${changedCount} changed`
                      }
                      key={lane.id}
                      label={lane.id}
                      onClick={() => void selectLane(lane.id)}
                      rightSection={
                        <Badge color={changedCount > 0 ? "yellow" : "gray"} size="sm" variant="light">
                          {lane.id === "base" ? view.files.length : changedCount}
                        </Badge>
                      }
                      variant="light"
                    />
                  );
                })}
              </Stack>
            </ScrollArea.Autosize>
          </Stack>

          <Stack gap="xs" mt="auto">
            <TextInput
              aria-label="New lane id"
              value={newLane}
              onChange={(event) => setNewLane(event.currentTarget.value)}
            />
            <Button fullWidth onClick={createLane} variant="light">
              Add lane
            </Button>
          </Stack>
        </Stack>
      </AppShell.Navbar>

      <AppShell.Main>
        <Stack gap="md" h="calc(100vh - 32px)">
          <Group align="flex-start" justify="space-between">
            <div>
              <Group gap="xs">
                <Badge color="gray" variant="light">
                  {activeLane}
                </Badge>
                <Text c="dimmed" size="sm">
                  {view.changedFiles.length} changed files
                </Text>
              </Group>
              <Title order={2} size="h3">
                {view.activeFile?.path ?? "No file selected"}
              </Title>
            </div>
            <Group>
              <Button onClick={reset} variant="default">
                Reset
              </Button>
              <Button color="green" disabled={!view.canEdit || !activeFileChanged} onClick={promoteFile}>
                Promote file
              </Button>
              <Button
                color="green"
                disabled={!view.canEdit || view.changedFiles.length === 0}
                onClick={promoteLane}
              >
                {promoteLaneLabel}
              </Button>
            </Group>
          </Group>

          <Tabs
            color="gray"
            value={view.activeFile?.path ?? null}
            onChange={(value) => {
              if (value) {
                void selectFile(value);
              }
            }}
          >
            <Tabs.List>
              {view.files.map((file) => {
                const changed = view.changedPaths.has(file.path);
                return (
                  <Tabs.Tab
                    key={file.path}
                    rightSection={
                      <Badge color={changed ? "yellow" : "gray"} size="xs" variant="light">
                        {changed ? "changed" : "base"}
                      </Badge>
                    }
                    value={file.path}
                  >
                    {file.path}
                  </Tabs.Tab>
                );
              })}
            </Tabs.List>
          </Tabs>

          <Textarea
            aria-label="Lane content"
            className="editor"
            disabled={!view.canEdit}
            minRows={18}
            resize="none"
            spellCheck={false}
            value={buffer}
            onChange={(event) => setBuffer(event.currentTarget.value)}
          />
        </Stack>
      </AppShell.Main>
    </AppShell>
  );
}

function deriveAppView(
  state: LaneState | null,
  activePath: string,
  activeLane: string,
  buffer: string,
): AppView {
  const files = state?.files ?? [];
  const activeFile = files.find((file) => file.path === activePath) ?? files[0] ?? null;
  const laneView =
    activeFile && activeLane === "base"
      ? activeFile.base
      : (activeFile?.lanes.find((lane) => lane.id === activeLane) ?? null);
  const activeView = laneView ?? activeFile?.base ?? null;
  const canEdit = Boolean(activeFile && activeLane !== "base" && laneView);
  const activeDraftPath = canEdit ? activeFile?.path : null;

  const changedFiles =
    activeLane === "base"
      ? []
      : files.filter((file) => {
          const content =
            file.path === activeDraftPath
              ? buffer
              : (file.lanes.find((lane) => lane.id === activeLane)?.content ?? file.base.content);
          return content !== file.base.content;
        });
  const changedPaths = new Set(changedFiles.map((file) => file.path));
  const changedCountByLane = new Map<string, number>([["base", 0]]);

  for (const file of files) {
    for (const lane of file.lanes) {
      const content = file.path === activeDraftPath && lane.id === activeLane ? buffer : lane.content;
      if (content !== file.base.content) {
        changedCountByLane.set(lane.id, (changedCountByLane.get(lane.id) ?? 0) + 1);
      }
    }
  }

  return {
    files,
    lanes: activeFile ? [activeFile.base, ...activeFile.lanes] : [],
    activeFile,
    activeView,
    canEdit,
    changedFiles,
    changedPaths,
    changedCountByLane,
  };
}

function lanePath(lane: string, suffix = "") {
  return `/api/lanes/${encodeURIComponent(lane)}${suffix}`;
}

async function apiJson<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(
    path,
    body === undefined
      ? undefined
      : {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(body),
        },
  );
  return parseResponse<T>(response);
}

async function parseResponse<T>(response: Response): Promise<T> {
  if (!response.ok) {
    const payload = (await response.json().catch(() => null)) as { error?: string } | null;
    throw new Error(payload?.error ?? `HTTP ${response.status}`);
  }
  return response.json() as Promise<T>;
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <MantineProvider theme={theme}>
      <App />
    </MantineProvider>
  </StrictMode>,
);
