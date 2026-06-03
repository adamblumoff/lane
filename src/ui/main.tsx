import { StrictMode, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Alert,
  Badge,
  Button,
  createTheme,
  Divider,
  Group,
  MantineProvider,
  MultiSelect,
  NavLink,
  ScrollArea,
  Stack,
  Text,
  TextInput,
  Textarea,
  Title,
} from "@mantine/core";
import "@mantine/core/styles.css";
import "./styles.css";
import {
  deriveReviewState,
  type LaneColumn,
  type LaneState,
  type ReviewLine,
} from "./compare";

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

function App() {
  const [state, setState] = useState<LaneState | null>(null);
  const [activeLane, setActiveLane] = useState(DEFAULT_LANE);
  const [activePath, setActivePath] = useState(DEFAULT_PATH);
  const [selectedLaneIds, setSelectedLaneIds] = useState([DEFAULT_LANE]);
  const [buffer, setBuffer] = useState("");
  const [newLane, setNewLane] = useState(DEFAULT_NEW_LANE);
  const [error, setError] = useState("");
  const syncedContent = useRef("");

  useEffect(() => {
    void refresh();
  }, []);

  const review = useMemo(
    () =>
      deriveReviewState(
        state,
        activePath,
        activeLane,
        selectedLaneIds,
        buffer,
      ),
    [activeLane, activePath, buffer, selectedLaneIds, state],
  );
  const activeSourceContent = useMemo(
    () => laneSourceContent(review.activeFile, activeLane),
    [activeLane, review.activeFile],
  );

  useEffect(() => {
    if (activeSourceContent === null) {
      return;
    }
    syncedContent.current = activeSourceContent;
    setBuffer(activeSourceContent);
  }, [activeLane, review.activePath, activeSourceContent]);

  useEffect(() => {
    if (!review.canEdit || !review.activeFile || buffer === syncedContent.current) {
      return;
    }

    const path = review.activeFile.path;
    const timeout = window.setTimeout(() => {
      void saveLane(activeLane, path, buffer);
    }, 450);

    return () => window.clearTimeout(timeout);
  }, [activeLane, buffer, review.activeFile, review.canEdit]);

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
    if (!review.canEdit || !review.activeFile || buffer === syncedContent.current) {
      return true;
    }
    return saveLane(activeLane, review.activeFile.path, buffer);
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
    if (review.laneIds.includes(lane)) {
      callUpLane(lane);
      setActiveLane(lane);
      return;
    }

    if (await runRequest(() => apiJson<LaneState>(lanePath(lane), {}))) {
      callUpLane(lane);
      setActiveLane(lane);
    }
  }

  async function setReviewLanes(nextLaneIds: string[]) {
    if (!(await saveActiveEdit())) {
      return;
    }
    const normalizedLaneIds = nextLaneIds.filter((laneId) =>
      review.laneIds.includes(laneId),
    );
    const addedLane = normalizedLaneIds.find(
      (laneId) => !selectedLaneIds.includes(laneId),
    );
    setSelectedLaneIds(normalizedLaneIds);
    if (addedLane) {
      setActiveLane(addedLane);
    } else if (
      activeLane !== "base" &&
      !normalizedLaneIds.includes(activeLane)
    ) {
      setActiveLane(normalizedLaneIds[0] ?? "base");
    }
  }

  function callUpLane(lane: string) {
    setSelectedLaneIds((laneIds) =>
      laneIds.includes(lane) ? laneIds : [...laneIds, lane],
    );
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
    if (!review.activeFile) {
      return;
    }
    const path = review.activeFile.path;
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
      setSelectedLaneIds([DEFAULT_LANE]);
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

  const promoteLaneLabel = `Promote lane (${review.activeLaneChangedFileCount} ${
    review.activeLaneChangedFileCount === 1 ? "file" : "files"
  })`;
  const changedLaneCount = review.columns.filter((column) => column.changed).length;
  const changedLaneText = changedLaneLabel(changedLaneCount);
  const laneOptions = review.laneIds.map((laneId) => ({
    value: laneId,
    label: laneId,
  }));

  return (
    <div className="app-frame">
      <aside className="sidebar">
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
              FILES
            </Text>
            <ScrollArea.Autosize mah="calc(100vh - 270px)">
              <Stack gap={4}>
                {review.fileSummaries.map((file) => {
                  return (
                    <NavLink
                      active={file.path === review.activePath}
                      description={changedLaneLabel(file.changedLaneCount)}
                      key={file.path}
                      label={file.path}
                      onClick={() => void selectFile(file.path)}
                      rightSection={
                        <Badge
                          color={file.changedLaneCount > 0 ? "yellow" : "gray"}
                          size="sm"
                          variant="light"
                        >
                          {file.changedLaneCount}
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
              Create lane
            </Button>
          </Stack>
        </Stack>
      </aside>

      <main className="main-pane">
        <Stack gap="md" h="100%">
          <Group align="flex-start" justify="space-between">
            <div>
              <Group gap="xs">
                <Badge color="gray" variant="light">
                  {activeLane}
                </Badge>
                <Text c="dimmed" size="sm">
                  {changedLaneText}
                </Text>
              </Group>
              <Title order={2} size="h3">
                {review.activeFile?.path ?? "No file selected"}
              </Title>
            </div>
            <Group>
              <Group className="lane-picker-wrap" gap="xs" wrap="nowrap">
                <MultiSelect
                  aria-label="Lanes to compare"
                  className="lane-picker"
                  data={laneOptions}
                  disabled={laneOptions.length === 0}
                  searchable
                  value={review.visibleLaneIds}
                  onChange={(nextLaneIds) => void setReviewLanes(nextLaneIds)}
                />
                {review.laneIds.length > 2 ? (
                  <Badge className="lane-count" color="gray" variant="light">
                    {review.visibleLaneIds.length}/{review.laneIds.length}
                  </Badge>
                ) : null}
              </Group>
              <Button onClick={reset} variant="default">
                Reset
              </Button>
              <Button
                color="green"
                disabled={!review.canEdit || !review.activeLaneChanged}
                onClick={promoteFile}
              >
                Promote file
              </Button>
              <Button
                color="green"
                disabled={!review.canEdit || review.activeLaneChangedFileCount === 0}
                onClick={promoteLane}
              >
                {promoteLaneLabel}
              </Button>
            </Group>
          </Group>

          <div className="review-shell">
            <div className="review-grid">
              {review.columns.map((column) => (
                <LaneColumnView
                  active={column.id === activeLane}
                  column={column}
                  key={column.id}
                  onSelect={() => void selectLane(column.id)}
                  onTextChange={setBuffer}
                  value={column.editable ? buffer : column.content}
                />
              ))}
            </div>
          </div>
        </Stack>
      </main>
    </div>
  );
}

type LaneColumnViewProps = {
  active: boolean;
  column: LaneColumn;
  value: string;
  onSelect: () => void;
  onTextChange: (value: string) => void;
};

function LaneColumnView({
  active,
  column,
  value,
  onSelect,
  onTextChange,
}: LaneColumnViewProps) {
  const className = [
    "lane-column",
    active ? "lane-column-active" : "",
    column.changed ? "lane-column-changed" : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <section className={className}>
      <div className="lane-column-header">
        <button className="lane-column-select" onClick={onSelect} type="button">
          <Group justify="space-between" gap="sm" wrap="nowrap">
            <Text fw={700} truncate>
              {column.id}
            </Text>
            <Badge
              color={column.changed ? "yellow" : "gray"}
              size="sm"
              variant={column.changed ? "light" : "outline"}
            >
              {column.changed ? "changed" : "base"}
            </Badge>
          </Group>
          <Text c="dimmed" size="xs">
            {column.byteLen} bytes
          </Text>
        </button>
      </div>

      {column.editable ? (
        <Textarea
          aria-label={`${column.id} lane content`}
          className="review-editor"
          minRows={18}
          resize="none"
          spellCheck={false}
          value={value}
          onChange={(event) => onTextChange(event.currentTarget.value)}
        />
      ) : (
        <CodePanel lines={column.lines} onSelect={onSelect} />
      )}
    </section>
  );
}

function CodePanel({
  lines,
  onSelect,
}: {
  lines: ReviewLine[];
  onSelect: () => void;
}) {
  return (
    <pre className="code-panel" onClick={onSelect}>
      {lines.map((line) => (
        <span
          className={[
            "code-line",
            line.changed ? "code-line-changed" : "",
            line.missing ? "code-line-missing" : "",
          ]
            .filter(Boolean)
            .join(" ")}
          key={line.number}
        >
          <span className="code-line-number">{line.number}</span>
          <span className="code-line-text">{line.text || " "}</span>
        </span>
      ))}
    </pre>
  );
}

function lanePath(lane: string, suffix = "") {
  return `/api/lanes/${encodeURIComponent(lane)}${suffix}`;
}

function changedLaneLabel(count: number) {
  return `${count} changed ${count === 1 ? "lane" : "lanes"}`;
}

function laneSourceContent(
  file: LaneState["files"][number] | null,
  lane: string,
) {
  if (!file) {
    return null;
  }
  if (lane === "base") {
    return file.base.content;
  }
  return file.lanes.find((view) => view.id === lane)?.content ?? file.base.content;
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
