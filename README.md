# lane

[![CI](https://github.com/adamblumoff/lane/actions/workflows/ci.yml/badge.svg)](https://github.com/adamblumoff/lane/actions/workflows/ci.yml)

Lane is a Windows-first CLI and Codex skill for file-level isolation between AI
coding agents. It lets multiple agents edit the same repo files in parallel
without copying the repo or creating git worktrees.

Lane is for agent workflows, not a human project-management UI. The human
installs the CLI, enables the bundled skill, and asks the agent to use Lane; the
agent handles runs, checks, review evidence, acceptance, and cleanup.

## Status

Pre-alpha. Expect destructive changes and breaking command contracts.

`lane run` and `lane check` currently target Windows and require the WinFsp
virtual filesystem so each agent can work inside a mounted lane view.

## Why Lane Exists

Git worktrees isolate whole repos. That is too coarse when several AI coding
agents are making different edits against the same files.

Lane moves the isolation boundary down to file operations:

- run asynchronous agent attempts in separate lanes
- review edits against the same base files
- accept clean operations directly
- handle conflicts per operation with explicit replacement bytes
- keep the base repo untouched until a parent agent accepts work

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
lane run login --attempts 5 -- <agent-command>
lane check login --name test -- <check-command>
lane review login --human
lane accept login-3
lane discard login
```

`lane run <name> --attempts <N>` reserves fresh attempt lanes named
`<name>-1`, `<name>-2`, and so on, runs the same command in each mounted lane
view, captures changed bytes, and stores attempt output under `.lane/runs`.

`lane check` runs a verification command inside every attempt lane and records
the check output without keeping check-generated files as attempt edits.

`lane review` combines attempt output, check results, and operation review into
one neutral evidence surface. It does not rank attempts or choose a winner.
Acceptance remains explicit through the emitted `lane accept` command arrays.

## Single-Lane Flow

```powershell
lane run agent-a -- <agent-command>
lane review agent-a --diff
lane review agent-a --human
lane accept agent-a
```

If the lane is not worth keeping:

```powershell
lane discard agent-a
```

## Commands

| Command | Purpose |
| --- | --- |
| `run <lane> -- <command>` | Run one command inside a mounted lane view. |
| `run <name> --attempts <N> -- <command>` | Run N isolated attempts for the same command. |
| `check <run> --name <name> -- <command>` | Run a verification command across every attempt without keeping check artifacts as attempt edits. |
| `review --history` | List stored runs and their check counts. |
| `review <run> [--human]` | Review attempts, checks, and lane state for one run. |
| `review [lane] [--human]` | Emit the structured lane review graph as JSON or human text. |
| `review <lane> --diff [paths...]` | Show a text diff for lane changes. |
| `review <lane> <path> <op-id>` | Expand one operation with byte previews. |
| `accept <lane>` | Accept every non-conflicting operation in a lane. |
| `accept <lane> <path> <ops...>` | Accept exact operations from one lane. |
| `accept <lane> <path> <op-id> --with-file <path>` | Accept one operation using replacement bytes. |
| `accept <path> --op <lane:op>... --with-file <path>` | Accept a conflict group using one replacement byte sequence and consume the selected source ops. |
| `discard <lane-or-run>` | Remove one lane or one stored run and its attempt lanes. |
| `doctor [--cleanup]` | Validate Lane storage; optionally delete unreferenced blobs. |

Most commands emit JSON by default so parent agents can make deterministic
decisions. `review --diff`, `review --human`, and `review <run> --human` are the
human-readable surfaces.

## Mental Model

The repo on disk is the base. A lane is a private overlay of file operations
against that base.

`lane run` gives a worker a normal-looking mounted repo, captures what changed,
and stores those changes in `.lane`.

`lane review` is the decision surface. Clean operations can be accepted
automatically. Conflicting operations can be expanded as detail, accepted with
replacement bytes, combined into one explicit replacement, selected exactly, or
discarded.

## Development

```powershell
cargo fmt
cargo test
lane doctor
```

Tests live outside `src/` and should preserve real manual workflows that are
important enough to keep running in the future.
