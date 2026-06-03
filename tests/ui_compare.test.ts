import { describe, expect, test } from "vitest";

import { deriveReviewState, type LaneState } from "../src/ui/compare.ts";

describe("deriveReviewState", () => {
  test("starts with only called-up lanes beside base", () => {
    const review = deriveReviewState(
      sampleState(),
      "demo/example.ts",
      "agent-a",
      ["agent-a"],
      "export const mode = 'fast';\n",
    );

    expect(
      review.columns.map((column) => ({
        id: column.id,
        changed: column.changed,
        editable: column.editable,
      })),
    ).toEqual([
      { id: "base", changed: false, editable: false },
      { id: "agent-a", changed: true, editable: true },
    ]);
    expect(review.laneIds).toEqual(["agent-a", "agent-b", "agent-c"]);
    expect(review.visibleLaneIds).toEqual(["agent-a"]);
    expect(
      review.columns[0].lines
        .filter((line) => line.changed)
        .map((line) => line.number),
    ).toEqual([1]);
    expect(review.fileSummaries[0].changedLaneCount).toBe(2);
    expect(review.fileSummaries[1].changedLaneCount).toBe(1);
  });

  test("calling up another lane expands the same-file comparison", () => {
    const review = deriveReviewState(
      sampleState(),
      "demo/example.ts",
      "agent-b",
      ["agent-a", "agent-b"],
      "export const mode = 'safe';\n",
    );

    expect(
      review.columns.map((column) => ({
        id: column.id,
        changed: column.changed,
        editable: column.editable,
      })),
    ).toEqual([
      { id: "base", changed: false, editable: false },
      { id: "agent-a", changed: true, editable: false },
      { id: "agent-b", changed: true, editable: true },
    ]);
    expect(review.visibleLaneIds).toEqual(["agent-a", "agent-b"]);
  });

  test("unsaved active edit updates lane and file change counts before persistence", () => {
    const review = deriveReviewState(
      sampleState(),
      "demo/example.ts",
      "agent-a",
      ["agent-a"],
      "export const mode = 'base';\n",
    );

    expect(review.columns.find((column) => column.id === "agent-a")?.changed).toBe(
      false,
    );
    expect([...review.activeLaneChangedPaths]).toEqual([]);
    expect(review.fileSummaries[0].changedLaneIds).toEqual(["agent-b"]);
    expect(review.fileSummaries[1].changedLaneIds).toEqual(["agent-b"]);
  });
});

function sampleState(): LaneState {
  return {
    storage_path: ".lane/repo.lane",
    files: [
      {
        path: "demo/example.ts",
        base: lane("base", "export const mode = 'base';\n"),
        lanes: [
          lane("agent-a", "export const mode = 'fast';\n"),
          lane("agent-b", "export const mode = 'safe';\n"),
          lane("agent-c", "export const mode = 'base';\n"),
        ],
      },
      {
        path: "demo/config.json",
        base: lane("base", '{\n  "mode": "base"\n}\n'),
        lanes: [
          lane("agent-a", '{\n  "mode": "base"\n}\n'),
          lane("agent-b", '{\n  "mode": "safe"\n}\n'),
          lane("agent-c", '{\n  "mode": "base"\n}\n'),
        ],
      },
    ],
  };
}

function lane(id: string, content: string) {
  return {
    id,
    content,
    byte_len: new TextEncoder().encode(content).length,
  };
}
