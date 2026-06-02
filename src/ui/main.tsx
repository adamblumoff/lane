import { StrictMode, useEffect, useMemo, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import "./styles.css";

type LaneView = {
  id: string;
  content: string;
  byte_len: number;
};

type LaneState = {
  file_path: string;
  base: LaneView;
  lanes: LaneView[];
};

function App() {
  const [state, setState] = useState<LaneState | null>(null);
  const [activeLane, setActiveLane] = useState("agent-a");
  const [buffer, setBuffer] = useState("");
  const [newLane, setNewLane] = useState("agent-c");
  const [error, setError] = useState("");
  const syncedContent = useRef("");

  useEffect(() => {
    void refresh();
  }, []);

  const activeView = useMemo(() => {
    if (!state) {
      return null;
    }
    if (activeLane === "base") {
      return state.base;
    }
    return state.lanes.find((lane) => lane.id === activeLane) ?? state.base;
  }, [activeLane, state]);

  const activeLaneExists = activeLane === "base" || Boolean(activeView?.id === activeLane);
  const canEdit = activeLane !== "base" && activeLaneExists;

  useEffect(() => {
    if (!activeView) {
      return;
    }
    syncedContent.current = activeView.content;
    setBuffer(activeView.content);
  }, [activeLane, activeView]);

  useEffect(() => {
    if (!canEdit || buffer === syncedContent.current) {
      return;
    }

    const timeout = window.setTimeout(() => {
      void saveLane(activeLane, buffer);
    }, 450);

    return () => window.clearTimeout(timeout);
  }, [activeLane, buffer, canEdit]);

  async function refresh() {
    await runRequest(() => apiGet<LaneState>("/api/state"));
  }

  async function saveLane(lane: string, content: string) {
    await runRequest(
      () =>
        apiPost<LaneState>(`/api/lanes/${encodeURIComponent(lane)}/replace`, {
          content,
        }),
    );
    syncedContent.current = content;
  }

  async function createLane() {
    const lane = newLane.trim();
    if (!lane || lane === "base") {
      setError("Use a lane id other than base.");
      return;
    }
    if (state?.lanes.some((existingLane) => existingLane.id === lane)) {
      setActiveLane(lane);
      return;
    }

    await runRequest(
      () =>
        apiPost<LaneState>(`/api/lanes/${encodeURIComponent(lane)}/replace`, {
          content: state?.base.content ?? "",
        }),
    );
    setActiveLane(lane);
  }

  async function promote() {
    if (canEdit && buffer !== syncedContent.current) {
      await saveLane(activeLane, buffer);
    }
    await runRequest(
      () => apiPost<LaneState>(`/api/lanes/${encodeURIComponent(activeLane)}/promote`, {}),
    );
    setActiveLane("base");
  }

  async function reset() {
    await runRequest(() => apiPost<LaneState>("/api/reset", {}));
    setActiveLane("agent-a");
  }

  async function runRequest(request: () => Promise<LaneState>) {
    try {
      const nextState = await request();
      setState(nextState);
      setError("");
    } catch (requestError) {
      setError(requestError instanceof Error ? requestError.message : "Request failed");
    }
  }

  const lanes = state ? [state.base, ...state.lanes] : [];

  return (
    <main className="shell">
      <aside className="rail">
        <header>
          <h1>Lane</h1>
        </header>

        {error ? <div className="error">{error}</div> : null}

        <nav aria-label="Lane views">
          {lanes.map((lane) => (
            <button
              className={lane.id === activeLane ? "lane active" : "lane"}
              key={lane.id}
              type="button"
              onClick={() => setActiveLane(lane.id)}
            >
              <span>{lane.id}</span>
              <small>{lane.byte_len}b</small>
            </button>
          ))}
        </nav>

        <div className="new-lane">
          <input
            aria-label="New lane id"
            value={newLane}
            onChange={(event) => setNewLane(event.target.value)}
          />
          <button type="button" onClick={createLane}>
            Add lane
          </button>
        </div>
      </aside>

      <section className="file">
        <header className="filebar">
          <div>
            <p>{activeLane}</p>
            <h2>{state?.file_path ?? ""}</h2>
          </div>
          <div className="actions">
            <button type="button" onClick={reset}>
              Reset
            </button>
            <button type="button" onClick={promote} disabled={!canEdit}>
              Promote
            </button>
          </div>
        </header>

        <textarea
          aria-label="Lane content"
          disabled={!canEdit}
          spellCheck={false}
          value={buffer}
          onChange={(event) => setBuffer(event.target.value)}
        />
      </section>
    </main>
  );
}

async function apiGet<T>(path: string): Promise<T> {
  const response = await fetch(path);
  return parseResponse<T>(response);
}

async function apiPost<T>(path: string, body: unknown): Promise<T> {
  const response = await fetch(path, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
  });
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
    <App />
  </StrictMode>,
);
