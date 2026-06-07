---
name: lane-orchestrate
description: Run multiple isolated implementation attempts in Lane and promote the best result. Use when the user asks Codex to try several approaches, variants, designs, fixes, prototypes, experiments, or agent attempts in parallel inside one repo without worktrees; especially for prompts like "use lane", "try 5 designs", "compare approaches", "pick the best lane", or "run subagents in lanes".
---

# Lane Orchestrate

Use Lane as the file-versioning layer, not as a planning-file format. The user-facing flow is prompt -> `lane exec` attempts -> virtual mounted repo view -> deterministic file capture -> compare -> promote.

## Workflow

1. Confirm the repo has `lane` available with `lane --help`. If needed, use the workspace binary at `target\debug\lane.exe`.
2. Choose short lane ids that name the attempted approach, such as `login-minimal`, `login-enterprise`, or `fix-parser-a`.
3. Launch each attempt through `lane exec <lane> -- <agent-or-command>`.
4. `lane exec` mounts a lane-specific virtual repo view, runs the worker with its current directory set to that mount, captures changed bytes back into the lane, leaves the base repo untouched, and prints JSON.
5. Do not ask the user to write or approve a JSON plan file.
6. Parse the JSON emitted by `lane exec`. It reports `mode: virtual_mount`, `workspace_root`/`mount_path`, `projected_paths`, worker-touched `changed_paths`, timings, and effective lane `changes`.
7. For promising lanes, inspect `lane review [lane]` for the JSON decision graph, use `lane diff <lane>` for human-readable patch review, and run verification commands through `lane exec`.
8. Pick the winner from evidence: tests, build output, screenshots, diffs, and fit to the user request.
9. Promote only the selected result with `lane promote-lane <lane>`, selected files with `lane promote <lane> <path>`, or selected operations with `lane promote-ops <lane> <path> <op-id>...`.
10. Discard losing lanes with `lane discard <lane>` once their useful evidence has been reported.

## Guardrails

- Do not create an intermediate plan artifact or ask the worker to output a write-set. Lane should interpret real file changes deterministically.
- Treat `lane exec` as the normal capture path. It is Codex-compatible and gives each worker its own mounted file view.
- If `lane exec` returns a non-zero `exit_code` or `worker_error`, inspect its JSON output before comparing or promoting that lane.
- Structured commands are JSON by default; `lane diff` is the text review command.
- Use `lane review` to compare clean ops and conflict groups before choosing `promote-clean`, `promote-ops`, or `resolve-op`.
- Keep the parent agent responsible for comparison and promotion. Subagents should implement their assigned variant, run local checks when asked, and summarize what changed.
- Preserve the normal repo until promotion. Before promotion, base files changing is a product failure unless the user explicitly made those edits outside Lane.
- Expect `changed_paths` to include temporary files a worker touched. Use `changes` and `lane diff <lane>` for the effective lane diff.

## Example Shape

For "try 5 login page designs and choose the best one":

```powershell
lane exec login-minimal -- codex exec --prompt "Implement a minimal login page."
lane exec login-enterprise -- codex exec --prompt "Implement an enterprise SaaS login page."
lane exec login-playful -- codex exec --prompt "Implement a more playful login page."
lane exec login-split -- codex exec --prompt "Implement a split-panel login page."
lane exec login-focused -- codex exec --prompt "Implement a focused conversion login page."
```

Then compare:

```powershell
lane diff login-enterprise
lane exec login-enterprise -- pnpm test
lane exec login-enterprise -- pnpm build
```

Finally:

```powershell
lane review
lane promote-lane login-enterprise
lane discard login-minimal
lane discard login-playful
lane discard login-split
lane discard login-focused
```
