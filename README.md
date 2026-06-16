# lane

[![CI](https://github.com/adamblumoff/lane/actions/workflows/ci.yml/badge.svg)](https://github.com/adamblumoff/lane/actions/workflows/ci.yml)

Lane is a Windows-first CLI and Codex skill for file-level isolation between AI
coding agents. It lets multiple agents edit the same repo files in parallel
without copying the repo or creating git worktrees.

Lane is for agent workflows, not a human project-management UI. The human
installs the CLI, enables the bundled skill, and asks the agent to use Lane; the
agent handles attempts, checks, comparison, promotion, and cleanup.

## Status

Pre-alpha. Expect destructive changes and breaking command contracts.

`lane exec`, `lane try`, and `lane check` currently target Windows and require
the WinFsp virtual filesystem so each agent can run inside a mounted lane view.

## Why Lane Exists

Git worktrees isolate whole repos. That is too coarse when several AI coding
agents are trying different edits against the same files.

Lane moves the isolation boundary down to file operations:

- run asynchronous agent attempts in separate lanes
- compare edits against the same base files
- promote clean operations directly
- resolve conflicts per operation instead of per copied repo
- keep the base repo untouched until a parent agent explicitly promotes work

If you are looking for an AI agent workflow tool, multi-agent coding CLI,
file-level version control experiment, git worktree alternative, virtual
filesystem overlay, or Codex orchestration skill, Lane is that experiment.

## Agent-First Quickstart

Build and test the CLI:

```powershell
cargo build
cargo test
```

Install the development binary on your path:

```powershell
cargo install --path .
```

Install the bundled Codex skill by copying it into your Codex skills directory:

```powershell
$dest = Join-Path $env:USERPROFILE ".codex\skills\lane-orchestrate"
New-Item -ItemType Directory -Force $dest | Out-Null
Copy-Item .\skills\lane-orchestrate\* $dest -Recurse -Force
```

Then ask Codex to use the Lane Orchestrate skill. The normal agent flow is:

```powershell
lane try --name login --attempts 5 -- codex exec --prompt "Implement the login page."
lane check login --name test -- pnpm test
lane compare login --human
lane promote-clean login-3
lane discard-run login
```

`lane try` reserves fresh attempt lanes named `<run>-1`, `<run>-2`, and so on,
runs the same command in each lane, captures changed bytes, and stores attempt
output under `.lane/runs`.

`lane check` runs a verification command inside every attempt lane and records
the check output without keeping check-generated file changes as attempt edits.

`lane compare` combines attempt output, check results, and the normal operation
review into one neutral evidence surface. It does not rank attempts or choose a
winner. Promotion remains explicit through the emitted `promote-clean`,
`promote-ops`, `resolve-op`, and `resolve-ops` commands.

## Single-Lane Flow

```powershell
lane exec agent-a -- codex exec --prompt "Implement the change."
lane diff agent-a
lane review --human agent-a
lane promote-clean agent-a
```

If the lane is not worth keeping:

```powershell
lane discard agent-a
```

## Commands

| Command | Purpose |
| --- | --- |
| `exec <lane> -- <command>` | Run a command inside a mounted lane view. |
| `try --name <run> --attempts <N> -- <command>` | Run N isolated attempts for the same command. |
| `check <run> --name <name> -- <command>` | Run a verification command across every attempt without keeping check artifacts as attempt edits. |
| `runs` | List stored attempt runs and their check counts. |
| `compare <run> [--human]` | Compare attempts, checks, and review state for a run. |
| `discard-run <run>` | Remove a run and every recorded attempt lane. |
| `diff <lane> [paths...]` | Show a text diff for lane changes. |
| `review [lane]` | Emit the structured review graph as JSON. |
| `review --human [lane]` | Show a human-readable review. |
| `promote-clean <lane>` | Promote every non-conflicting operation. |
| `promote-ops <lane> <path> <ops...>` | Promote specific operations. |
| `show-op <lane> <path> <op-id>` | Inspect one operation with byte previews. |
| `resolve-op <lane> <path> <op-id> --with-file <path>` | Replace one operation with resolved bytes. |
| `resolve-ops <path> --op <lane:op>... --with-file <path>` | Replace a selected group of operations with one resolved byte sequence and consume the selected source ops. |
| `discard <lane>` | Remove a lane and its private changes. |
| `gc` | Delete unreferenced blobs from lane storage. |
| `doctor` | Validate Lane storage and report repairable state. |

Most commands emit JSON by default so parent agents can make deterministic
decisions. `diff`, `review --human`, and `compare --human` are the main
human-readable surfaces.

## Mental Model

The repo on disk is the base. A lane is a private overlay of file operations
against that base.

`lane exec` gives a worker a normal-looking mounted repo, captures what changed,
and stores those changes in `.lane`.

`lane review` and `lane compare` are decision points. Clean operations can be
promoted automatically. Conflicting operations can be inspected, resolved as a
single winner, combined into one explicit `resolve-ops` replacement, promoted
selectively, or discarded.

## Development

```powershell
cargo fmt
cargo test
lane doctor
```

Tests live outside `src/` and should preserve real manual workflows that are
important enough to keep running in the future.
