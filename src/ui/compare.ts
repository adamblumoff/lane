export type LaneView = {
  id: string;
  content: string;
  byte_len: number;
};

export type FileView = {
  path: string;
  base: LaneView;
  lanes: LaneView[];
};

export type LaneState = {
  storage_path: string;
  files: FileView[];
};

export type ReviewLine = {
  number: number;
  text: string;
  changed: boolean;
  missing: boolean;
};

export type LaneColumn = {
  id: string;
  content: string;
  byteLen: number;
  changed: boolean;
  editable: boolean;
  lines: ReviewLine[];
};

export type FileSummary = {
  path: string;
  changedLaneCount: number;
  changedLaneIds: string[];
};

export type ReviewState = {
  files: FileView[];
  fileSummaries: FileSummary[];
  activeFile: FileView | null;
  activePath: string;
  laneIds: string[];
  visibleLaneIds: string[];
  columns: LaneColumn[];
  activeColumn: LaneColumn | null;
  canEdit: boolean;
  activeLaneChanged: boolean;
  activeLaneChangedFileCount: number;
  activeLaneChangedPaths: Set<string>;
};

export function deriveReviewState(
  state: LaneState | null,
  requestedPath: string,
  activeLane: string,
  selectedLaneIds: string[],
  buffer: string,
): ReviewState {
  const files = state?.files ?? [];
  const activeFile =
    files.find((file) => file.path === requestedPath) ?? files[0] ?? null;
  const activePath = activeFile?.path ?? "";
  const laneIds = activeFile?.lanes.map((lane) => lane.id) ?? [];
  const visibleLaneIds = uniqueLaneIds(selectedLaneIds).filter((laneId) =>
    laneIds.includes(laneId),
  );
  const activeLaneView =
    activeFile && activeLane !== "base"
      ? (activeFile.lanes.find((lane) => lane.id === activeLane) ?? null)
      : null;
  const canEdit = Boolean(activeFile && activeLane !== "base" && activeLaneView);

  const contentFor = (file: FileView, laneId: string) => {
    if (laneId === "base") {
      return file.base.content;
    }
    if (canEdit && file.path === activePath && laneId === activeLane) {
      return buffer;
    }
    return file.lanes.find((lane) => lane.id === laneId)?.content ?? file.base.content;
  };

  const fileSummaries = files.map((file) => {
    const changedLaneIds = file.lanes
      .filter((lane) => contentFor(file, lane.id) !== file.base.content)
      .map((lane) => lane.id);
    return {
      path: file.path,
      changedLaneCount: changedLaneIds.length,
      changedLaneIds,
    };
  });

  const activeLaneChangedPaths = new Set(
    activeLane === "base"
      ? []
      : files
          .filter((file) => contentFor(file, activeLane) !== file.base.content)
          .map((file) => file.path),
  );

  const columns = activeFile
    ? buildColumns(activeFile, activeLane, visibleLaneIds, buffer, canEdit)
    : [];
  const activeColumn =
    columns.find((column) => column.id === activeLane) ?? columns[0] ?? null;

  return {
    files,
    fileSummaries,
    activeFile,
    activePath,
    laneIds,
    visibleLaneIds,
    columns,
    activeColumn,
    canEdit,
    activeLaneChanged: Boolean(
      activeFile && activeLaneChangedPaths.has(activeFile.path),
    ),
    activeLaneChangedFileCount: activeLaneChangedPaths.size,
    activeLaneChangedPaths,
  };
}

function buildColumns(
  file: FileView,
  activeLane: string,
  selectedLaneIds: string[],
  buffer: string,
  canEdit: boolean,
) {
  const selectedLanes = selectedLaneIds
    .map((laneId) => file.lanes.find((lane) => lane.id === laneId))
    .filter((lane): lane is LaneView => Boolean(lane));
  const laneContents = selectedLanes.map((lane) => ({
    id: lane.id,
    content: canEdit && lane.id === activeLane ? buffer : lane.content,
  }));
  const baseLines = splitLines(file.base.content);
  const laneLines = laneContents.map((lane) => splitLines(lane.content));
  const maxLineCount = Math.max(
    baseLines.length,
    ...laneLines.map((lines) => lines.length),
  );
  const baseChangedLines = new Set<number>();

  for (let index = 0; index < maxLineCount; index += 1) {
    const baseLine = baseLines[index] ?? "";
    if (laneLines.some((lines) => (lines[index] ?? "") !== baseLine)) {
      baseChangedLines.add(index);
    }
  }

  return [
    {
      id: "base",
      content: file.base.content,
      byteLen: file.base.byte_len,
      changed: false,
      editable: false,
      lines: buildReviewLines(baseLines, baseLines, maxLineCount, (index) =>
        baseChangedLines.has(index),
      ),
    },
    ...selectedLanes.map((lane, laneIndex) => {
      const content =
        canEdit && lane.id === activeLane ? buffer : lane.content;
      const lines = laneLines[laneIndex] ?? splitLines(content);
      return {
        id: lane.id,
        content,
        byteLen: utf8ByteLength(content),
        changed: content !== file.base.content,
        editable: canEdit && lane.id === activeLane,
        lines: buildReviewLines(lines, baseLines, maxLineCount, (index) =>
          (lines[index] ?? "") !== (baseLines[index] ?? ""),
        ),
      };
    }),
  ];
}

function buildReviewLines(
  lines: string[],
  baseLines: string[],
  maxLineCount: number,
  changedAt: (index: number) => boolean,
): ReviewLine[] {
  return Array.from({ length: maxLineCount }, (_, index) => ({
    number: index + 1,
    text: lines[index] ?? "",
    changed: changedAt(index),
    missing: index >= lines.length && index < baseLines.length,
  }));
}

function splitLines(content: string) {
  const normalized = content.replace(/\r\n/g, "\n").replace(/\r/g, "\n");
  if (!normalized) {
    return [""];
  }
  const lines = normalized.split("\n");
  if (lines.at(-1) === "") {
    lines.pop();
  }
  return lines.length > 0 ? lines : [""];
}

function utf8ByteLength(content: string) {
  return new TextEncoder().encode(content).length;
}

function uniqueLaneIds(laneIds: string[]) {
  return Array.from(
    new Set(laneIds.filter((laneId) => laneId && laneId !== "base")),
  );
}
