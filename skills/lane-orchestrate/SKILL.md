---
name: lane-orchestrate
description: Run multiple isolated implementation attempts in Lane and promote the best result. Use when the user asks Codex to try several approaches, variants, designs, fixes, prototypes, experiments, or agent attempts in parallel inside one repo without worktrees; especially for prompts like "use lane", "try 5 designs", "compare approaches", "pick the best lane", or "run subagents in lanes".
---

# Lane Orchestrate

Use Lane as the file-versioning layer, not as a planning-file format. The user-facing flow is prompt -> `lane exec` attempts -> deterministic file capture -> compare -> promote.

## Workflow

1. Confirm the repo has `lane` available with `lane --help`. If needed, use the workspace binary at `target\debug\lane.exe`.
2. Choose short lane ids that name the attempted approach, such as `login-minimal`, `login-enterprise`, or `fix-parser-a`.
3. Launch each attempt through `lane exec <lane> -- <agent-or-command>`.
4. `lane exec` projects the lane into the real repo, lets the worker edit normal files, deterministically captures changed bytes back into the lane, restores the base repo, and prints JSON.
5. Do not ask the user to write or approve a JSON plan file.
6. Parse the JSON emitted by `lane exec`. It reports `mode: raw_repo`, restore state, changed paths, timings, and `changes`.
7. For promising lanes, inspect `lane diff <lane>` and run verification commands through `lane exec`.
8. Pick the winner from evidence: tests, build output, screenshots, diffs, and fit to the user request.
9. Promote only the selected result with `lane promote-lane <lane> --json`, or promote selected files with `lane promote <lane> <path> --json`.
10. Discard losing lanes with `lane discard <lane> --json` once their useful evidence has been reported.

## Guardrails

- Do not create an intermediate plan artifact or ask the worker to output a write-set. Lane should interpret real file changes deterministically.
- Treat `lane exec` as the normal capture path. It is Codex-compatible, but raw-repo execution is serialized during the worker run.
- If `lane exec` returns `restore_error`, stop and inspect the repo before comparing or promoting.
- Keep the parent agent responsible for comparison and promotion. Subagents should implement their assigned variant, run local checks when asked, and summarize what changed.
- Preserve the normal repo until promotion. Before promotion, base files changing is a product failure unless the user explicitly made those edits outside Lane.

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
lane promote-lane login-enterprise --json
lane discard login-minimal --json
lane discard login-playful --json
lane discard login-split --json
lane discard login-focused --json
```
