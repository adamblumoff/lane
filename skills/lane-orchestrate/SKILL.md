---
name: lane-orchestrate
description: Run multiple isolated implementation attempts in Lane and compare their evidence and operations. Use when the user asks Codex to try several approaches, variants, designs, fixes, prototypes, experiments, or agent attempts in parallel inside one repo without worktrees; especially for prompts like "use lane", "try 5 designs", "compare approaches", "judge the lanes", or "run subagents in lanes".
---

# Lane Orchestrate

Use Lane as the file-versioning layer, not as a planning-file format. The user-facing flow is prompt -> `lane try` attempts -> `lane check` verification -> `lane compare` decision surface -> promote or resolve selected ops.

## Workflow

1. Confirm the repo has `lane` available with `lane --help`. If needed, use the workspace binary at `target\debug\lane.exe`.
2. Choose a short run id that names the experiment, such as `login`, `fix-parser`, or `pricing-page`.
3. Launch attempts through `lane try --name <run> --attempts <N> -- <agent-or-command>`.
4. `lane try` reserves fresh `<run>-1`, `<run>-2`, etc. lanes, mounts lane-specific virtual repo views, captures changed bytes back into each lane, leaves the base repo untouched, stores `.lane/runs/<run>.json`, and prints JSON.
5. Do not ask the user to write or approve a JSON plan file.
6. Run important verification through `lane check <run> -- <check-command>`. It records check outputs without keeping check-generated files as attempt edits.
7. Use `lane compare <run>` for the JSON evidence graph or `lane compare <run> --human` for the human-readable review.
8. Judge the attempts from evidence: checks, build output, screenshots, diffs, conflicts, operation previews, and fit to the user request. Do not blindly choose by displayed order or metrics.
9. Use the commands emitted by `lane compare` and `lane review` to apply the judgment: run `promote-clean` for clean ops, `show-op` and `resolve-op --with-file <replacement-file>` for conflicted ops, and `promote-ops <lane> <path> <op-id>...` only when deliberately selecting exact clean ops.
10. Discard losing lanes by running their `discard` action once their useful evidence has been reported.

## Guardrails

- Do not create an intermediate plan artifact or ask the worker to output a write-set. Lane should interpret real file changes deterministically.
- Treat `lane try` as the normal multi-attempt capture path. It is Codex-compatible and gives each worker its own mounted file view.
- If an attempt returns a non-zero `exit_code` or `worker_error`, inspect its JSON output before comparing or promoting that lane.
- Structured commands are JSON by default; `lane diff` is the text review command.
- Use `lane review` to compare clean ops and conflict groups before choosing `promote-clean`, `promote-ops`, or `resolve-op`. Prefer executing the command arrays emitted by `review` so the parent workflow dogfoods the same contract it presents to agents.
- Keep the parent agent responsible for comparison and promotion. Subagents should implement their assigned variant, run local checks when asked, and summarize what changed.
- Preserve the normal repo until promotion. Before promotion, base files changing is a product failure unless the user explicitly made those edits outside Lane.
- Expect `changed_paths` to include temporary files a worker touched. Use `lane review [lane]` for the effective structured lane state and `lane diff <lane>` for the human-readable patch.

## Example Shape

For "try 5 login page designs and judge them":

```powershell
lane try --name login --attempts 5 -- codex exec --prompt "Implement a strong login page."
```

Then compare:

```powershell
lane check login --name test -- pnpm test
lane check login --name build -- pnpm build
lane compare login --human
```

Finally:

```powershell
lane promote-clean login-3
# For remaining conflicts, inspect the emitted actions and resolve only the selected op:
lane show-op login-3 src/login.tsx login-3:1
lane resolve-op login-3 src/login.tsx login-3:1 --with-file .\resolution.txt
lane discard login-1
lane discard login-2
lane discard login-4
lane discard login-5
```
