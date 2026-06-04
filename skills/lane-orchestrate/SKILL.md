---
name: lane-orchestrate
description: Run multiple isolated implementation attempts in Lane and promote the best result. Use when the user asks Codex to try several approaches, variants, designs, fixes, prototypes, experiments, or agent attempts in parallel inside one repo without worktrees; especially for prompts like "use lane", "try 5 designs", "compare approaches", "pick the best lane", or "run subagents in lanes".
---

# Lane Orchestrate

Use Lane as the isolation layer, not as a planning-file format. The user-facing flow is prompt -> lane-backed attempts -> compare -> promote.

## Workflow

1. Confirm the repo has `lane` available with `lane --help`. If needed, use the workspace binary at `target\debug\lane.exe`.
2. Choose short lane ids that name the attempted approach, such as `login-minimal`, `login-enterprise`, or `fix-parser-a`.
3. Launch each attempt through `lane exec <lane> -- <agent-or-command>`, so the worker runs with its cwd and `LANE_REPO_ROOT` set to the lane view.
4. Run attempts in parallel when their work is independent. Do not ask the user to write or approve a JSON plan file.
5. Parse the JSON emitted by each `lane exec`. It includes `exit_code`, `stdout`, `stderr`, `escaped`, `rolled_back`, `escaped_paths`, and `changes`.
6. For promising lanes, inspect `lane diff <lane>` and run any lane-local verification commands needed for the task through another `lane exec`.
7. Pick the winner from evidence: tests, build output, screenshots, diffs, and fit to the user request.
8. Promote only the selected result with `lane promote-lane <lane> --json`, or promote selected files with `lane promote <lane> <path> --json`.
9. Discard losing lanes with `lane discard <lane> --json` once their useful evidence has been reported.

## Guardrails

- Do not create an intermediate plan artifact. Prefer direct `lane exec` orchestration from the user prompt.
- Do not pass the real backing repo root to subagents. The child agent should see the lane view as its repo root.
- If `lane exec` returns `escaped: true`, stop and report `escaped_paths`. Current Lane rolls those base changes back before failing; if `rolled_back` is false, treat the repo as needing manual recovery before continuing.
- Keep the parent agent responsible for comparison and promotion. Subagents should implement their assigned variant, run local checks when asked, and summarize what changed.
- Preserve the normal repo until promotion. Before promotion, base files changing is a product failure unless the user explicitly made those edits outside Lane.

## Example Shape

For "try 5 login page designs and choose the best one":

```powershell
lane exec login-minimal -- codex exec --prompt "Implement a minimal login page. Work only in this lane view."
lane exec login-enterprise -- codex exec --prompt "Implement an enterprise SaaS login page. Work only in this lane view."
lane exec login-playful -- codex exec --prompt "Implement a more playful login page. Work only in this lane view."
lane exec login-split -- codex exec --prompt "Implement a split-panel login page. Work only in this lane view."
lane exec login-focused -- codex exec --prompt "Implement a focused conversion login page. Work only in this lane view."
```

Then compare:

```powershell
lane diff login-enterprise
lane exec login-enterprise -- pnpm test
lane exec login-enterprise -- pnpm build
```

Finally:

```powershell
lane promote-lane login-enterprise --json
lane discard login-minimal --json
lane discard login-playful --json
lane discard login-split --json
lane discard login-focused --json
```
