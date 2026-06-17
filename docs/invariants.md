# Lane Invariants

Lane is a byte-level state machine for agent work. The base worktree is the
shared state. Each lane is a private overlay of file operations that can be
reviewed, accepted, or discarded. These invariants are the claims new core,
storage, VFS, and orchestration changes must preserve.

## Core Repo

- User lanes are never empty and are never named `base`.
- Reading `base` returns the supplied base bytes without consulting overlays.
- Reading an untouched user lane returns the supplied base bytes.
- Replacing a path in a lane and then reading that lane returns the replacement
  bytes for the same base.
- Creating an empty file from a missing base is still a real create operation;
  it is reviewable, acceptable, and preserved by storage roundtrips.
- Deleting a present path in a lane and then reading that lane returns missing.
- A path overlay is tied to the base fingerprint captured when the overlay was
  created. Reads, reviews, and accepts against different base bytes fail with
  `BaseChanged`.
- Stored present entries contain valid operation order keys.
- Stored operations for one path/lane are ordered in base coordinates and do not
  overlap in a way that makes rendering ambiguous.
- Operation coordinate arithmetic never silently wraps.
- Same-offset pure inserts are deterministic and do not conflict.
- Overlapping replacements and deletes remain explicit alternatives until an
  accept command consumes or replaces them.
- Accepting selected clean operations mutates only the base bytes for those
  selected operations and removes only the consumed source operations.
- Retained lanes preserve their rendered intent after a non-conflicting accept.
  When coordinate rebasing cannot preserve intent, the retained lane is rebuilt
  as replacement content against the accepted base.
- A retained lane overlay that already renders to the accepted base is removed
  instead of replayed as a duplicate operation.
- Discarding a lane removes only that lane's overlays and leaves base bytes
  unchanged.

## Storage

- `storage_snapshot` followed by `from_storage_snapshot` preserves valid repo
  behavior.
- Invalid snapshots are rejected before they affect the loaded repo.
- Every overlay entry references a lane present in the manifest lane set.
- Reserved manifest lane names are rejected.
- Inserted blobs are content-addressed by SHA-256 and may be shared by many ops.
- Last-run records are advisory evidence. Corrupt last-run files do not make a
  valid repo unloadable, but `doctor` reports them.
- Cleanup never removes referenced blobs and refuses to clean unhealthy storage.

## Paths And VFS

- Internal repo paths are repo-relative labels using `/` separators.
- Absolute paths, parent traversal, empty file paths, and NUL-containing paths
  are rejected before projection or mutation.
- Root `.lane` state is never projectable to agents.
- Root `.git` metadata is never projectable or mutable through a lane view.
- Root `.lane` and `.git` matching is case-insensitive.
- Non-root `.git` path components are normal file paths unless a stricter
  product rule explicitly changes that contract.
- Case variants of one visible path collapse to the newest dirty entry during a
  virtual run.
- Directory/file replacement accepts apply as one transaction and roll back the
  worktree if storage persistence fails.

## Review And Orchestration

- `lane review` is evidence, not a ranking mechanism.
- Clean operations and conflict groups are derived from operation relations, not
  from worker ordering.
- Conflict-combine acceptance is allowed only for one conflict-connected group.
- `lane run --attempts` reserves fresh attempt lanes before workers run.
- The base worktree remains unchanged until an accept command commits selected
  bytes.
- `lane check` records process evidence with `persist_changes: false`; files
  produced by checks are not persisted as attempt edits.
- Run and check outputs are deterministic JSON evidence surfaces for parent
  agents.
- Stdout and stderr previews are valid UTF-8 slices and are bounded to the
  preview limit.

## Current Formalization Surface

- Rust types and error enums encode the public failure modes.
- `LaneId` is a validated newtype; empty lane names and `base` cannot be built
  through normal lane APIs.
- `FilePath` is a validated newtype; absolute paths, traversal, root `.lane`,
  root `.git`, NUL-containing paths, and empty file paths cannot enter normal
  core file APIs.
- Property tests cover byte-level laws over generated inputs, including
  roundtrips, stale-base rejection, and same-offset insert convergence.
- Stateful model tests cover generated one-path histories across two or three
  lanes for lane creation, whole-file replacement, deletion, accept, discard,
  storage roundtrip, stale-base rejection, and overlay isolation.
- Integration tests cover important CLI, storage, VFS, and orchestration flows.
- Future bounded models should focus on selected partial accepts and conflict
  replacement sequences for one file across two or three lanes.
