---
name: lane-orchestrate
description: Use Lane to run, verify, review, accept, and clean up isolated AI-agent implementation attempts in one repo without git worktrees or repo copies. Trigger when the user asks Codex to use Lane, run several variants, judge attempts, dogfood Lane, or run agents/subagents in parallel lanes.
---

# Lane Orchestrate

Use Lane as the file-versioning layer, not as a planning-file format. The normal flow is prompt -> `lane run --attempts` -> `lane check` verification -> `lane review` evidence -> `lane accept` selected work -> `lane discard` losing lanes or runs.

## Workflow

1. Confirm the repo has `lane` available with `lane --help`. If the command is missing in a Lane checkout, use `target\debug\lane.exe --help`. If neither works, build or install the CLI before orchestrating attempts.
2. Choose a short run id that names the experiment, such as `login`, `fix-parser`, or `pricing-page`.
3. Launch attempts through `lane run <run> --attempts <N> -- <agent-or-command>`.
4. Remember that `lane run --attempts` reserves fresh `<run>-1`, `<run>-2`, etc. lanes, mounts lane-specific virtual repo views, captures changed bytes back into each lane, leaves the base repo untouched, stores `.lane/runs/<run>.json`, and prints JSON.
5. Do not ask the user to write or approve a JSON plan file.
6. Run important verification through `lane check <run> --name <check-name> -- <check-command>`. It records check outputs without keeping check-generated files as attempt edits.
7. If `lane run` or `lane check` returns non-zero because one attempt failed, inspect the JSON and continue to `lane review` unless every attempt is unusable or Lane itself failed to produce run records.
8. Use `lane review <run>` for the JSON evidence graph or `lane review <run> --human` for the human-readable review.
9. Judge attempts from evidence: checks, build output, screenshots, diffs, conflicts, operation previews, and fit to the user request. Do not blindly choose by displayed order or metrics.
10. Use the command arrays emitted by `lane review` to apply the judgment. `lane accept <lane>` applies clean ops, `lane review <lane> <path> <op-id>` expands one operation, `lane accept <lane> <path> <op-id> --with-file <replacement-file>` chooses one conflicted op with replacement bytes, `lane accept <path> --op <lane:op>... --with-file <replacement-file>` combines a conflict group into one accepted replacement, and `lane accept <lane> <path> <op-id>...` applies exact clean ops.
11. Discard losing lanes by running their `discard` action, or remove the whole run with `lane discard <run>`, once useful evidence has been reported and selected work has been accepted.

## Guardrails

- Do not create an intermediate plan artifact or ask the worker to output a write-set. Lane should interpret real file changes deterministically.
- Treat `lane run --attempts` as the normal multi-attempt capture path. It is Codex-compatible and gives each worker its own mounted file view.
- If an attempt returns a non-zero `exit_code` or `worker_error`, inspect its JSON output before evaluating that lane.
- When a check fails for some attempts, treat that as evidence rather than an automatic reason to abandon the run.
- Structured commands are JSON by default; `lane review <lane> --diff` is the text patch command.
- Use `lane review` to inspect the ordered per-file `ops` list, clean ops, and conflict groups before choosing acceptance commands. Prefer running the emitted command arrays so the parent workflow dogfoods the same contract it presents to agents.
- Keep the parent agent responsible for judging and accepting work. Subagents should implement their assigned variant, run local checks when asked, and summarize what changed.
- Preserve the normal repo until acceptance. Before acceptance, base files changing is a product failure unless the user explicitly made those edits outside Lane.
- Expect `changed_paths` to include temporary files a worker touched. Use `lane review <lane>` for the effective structured lane state and `lane review <lane> --diff` for the human-readable patch.
- Use `lane doctor` when storage health is uncertain. Use `lane doctor --cleanup` only as cleanup for unreferenced blobs after acceptance, discard, or failed experimentation.

## Example Shape

For five login page designs:

```powershell
lane run login --attempts 5 -- <agent-command>
```

Then gather evidence:

```powershell
lane check login --name test -- pnpm test
lane check login --name build -- pnpm build
lane review login --human
```

Finally:

```powershell
lane accept login-3
# For one-winner conflicts, expand the emitted detail action and accept replacement bytes:
lane review login-3 src/login.tsx login-3:1
lane accept login-3 src/login.tsx login-3:1 --with-file .\replacement.txt
# When several conflicting chunks should all be kept, write the combined replacement bytes:
lane accept src/login.tsx --op login-1:1 --op login-3:1 --with-file .\combined-replacement.txt
lane discard login
```
