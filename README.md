# lane

Lane lets multiple agents work on the same repo files at the same time without
copying the repo or creating git worktrees.

It is a file-level isolation layer for agent work. Each agent writes into its own
lane, Lane records the file operations, and the base repo stays untouched until
you explicitly promote selected work.

## Status

Pre-alpha. Expect breaking changes.

Development currently targets Windows. `lane exec` requires the WinFsp virtual
filesystem so agents can run inside a mounted lane view.

## Why

Git worktrees isolate whole repos. That is too coarse when several agents are
trying different edits against the same files.

Lane moves the isolation boundary down to the file operation level:

- run agents asynchronously in separate lanes
- compare their edits against the same base files
- promote clean operations directly
- resolve conflicts per operation instead of per copied repo

## Build

```powershell
cargo build
cargo test
```

To put the `lane` binary on your path while developing:

```powershell
cargo install --path .
```

## Basic AX Flow

```powershell
lane exec agent-a -- codex exec --prompt "Implement the change."
lane diff agent-a
lane review --human
lane promote-clean agent-a
```

If the lane is not worth keeping:

```powershell
lane discard agent-a
```

## N-Attempt Flow

```powershell
lane try --name login --attempts 5 -- codex exec --prompt "Implement the login page."
lane check login --name test -- pnpm test
lane compare login --human
```

`lane try` reserves fresh attempt lanes named `<run>-1`, `<run>-2`, and so on,
runs the same command in each lane, and stores attempt output under `.lane/runs`.

`lane check` runs a verification command inside every attempt lane and records
the check output without keeping check-generated file changes as attempt edits.

`lane compare` combines attempt output, check results, and the normal operation
review into one neutral evidence surface. It does not rank attempts or choose a
winner. Promotion is still explicit through the copyable `promote-clean`,
`promote-ops`, and `resolve-op` commands it reports.

## Commands

| Command | Purpose |
| --- | --- |
| `exec <lane> -- <command>` | Run a command inside a mounted lane view. |
| `try --name <run> --attempts <N> -- <command>` | Run N isolated attempts for the same command. |
| `check <run> -- <command>` | Run a verification command across every attempt without keeping check artifacts as attempt edits. |
| `compare <run> [--human]` | Compare attempts, checks, and review state for a run. |
| `diff <lane> [paths...]` | Show a text diff for lane changes. |
| `review [lane]` | Emit the structured review graph as JSON. |
| `review --human [lane]` | Show a human-readable review. |
| `promote-clean <lane>` | Promote every non-conflicting operation. |
| `promote-ops <lane> <path> <ops...>` | Promote specific operations. |
| `show-op <lane> <path> <op-id>` | Inspect one operation with byte previews. |
| `resolve-op <lane> <path> <op-id> --with-file <path>` | Replace one operation with resolved bytes. |
| `discard <lane>` | Remove a lane and its private changes. |
| `doctor` | Validate Lane storage. |

## Mental Model

The repo on disk is the base.

A lane is a private overlay of file operations against that base.

`lane exec` gives a worker a normal-looking mounted repo, captures what changed,
and stores those changes in `.lane`.

`lane review` is the decision point. Clean operations can be promoted
automatically. Conflicting operations can be inspected, resolved, promoted
selectively, or discarded.

## Development

```powershell
cargo test
```

Tests live outside `src/` and should preserve real manual workflows that are
important enough to keep running.
