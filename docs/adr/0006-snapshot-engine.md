# ADR-0006: Snapshot engine design (§17 Q6, §5.5)

- Status: Accepted (Phase 3)
- Date: 2026-10-06
- Supersedes: ADR-0018 (interim pre-image store)

## Decision

Build it in-tree as `crates/anchor-snapshot`. The engine is the durability
foundation and the substrate of the merge queue. Delegating it would put
the core guarantee in someone else's format.

### Store

The store lives in the workspace's state directory, outside the workspace
(ADR-0022):

```
snapshots/
  lock                      flock (Unix) / exclusive open (Windows)
  objects/<aa>/<id hex>     immutable, content-addressed
  refs/HEAD                 last observed workspace state
  refs/turns/<seq:010>      pre-turn snapshots, numbered by the backend
  refs/restore/<ms>[-mid]   pre-restore state (and state at a failed restore)
  refs/manual/<ms>          `anchor snapshot create`
  index                     change-detection cache
  tmp/                      staging; wiped on open
  restore.journal           only while a restore is in flight
```

- **Objects.** An object is `[type byte][payload]`, and its id is the
  SHA-256 of those bytes, so an object cannot be accepted as another type.
  There are four types:
  - `Chunk`: raw bytes;
  - `File`: size, whole-content SHA-256, and ordered `(chunk id, length)`
    pairs;
  - `Tree`: sorted entries `File{id, mode} | Dir{id, mode} |
    Symlink{target} | Untracked{reason}`;
  - `Snapshot`: root, time, counts, label.
- **Encoding.** Fixed-width little-endian binary with one canonical
  encoding per value. Decoding rejects:
  - truncation and trailing bytes;
  - unknown versions or kinds;
  - unsorted or duplicate entries;
  - names that are empty, `.`, `..`, or contain `/` or NUL (on Windows also
    `\` and `:`);
  - mode bits outside `0o777` (setuid/setgid/sticky are never recreated,
    because restores may run as root);
  - chunk lengths that do not sum to the declared size.
- **Verification.** Every read re-hashes the object. A dedup hit on an
  existing content object re-reads and verifies it, and a damaged copy is
  replaced by the fresh one.
- **Refs** match `[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)?` and must name a
  `Snapshot`. Turn refs are numbered by the backend. The runtime's turn
  number goes only in the label (`session <id> turn <n>`), so a hostile
  runtime cannot overwrite a rollback point.

### Chunking

FastCDC with normalized chunking (Xia et al., IEEE TPDS 2020), implemented
in-tree in about 150 lines (`chunker.rs`):

- chunk sizes are min 16 KiB, average 64 KiB, max 256 KiB;
- masks use 18 and 14 bits spread over the high 48 bits of the gear hash;
- the gear table comes from a fixed SplitMix64 seed;
- files of 64 KiB or less are stored as a single chunk.

Tests:

- determinism, size bounds, and an average within 32–128 KiB;
- a 1-byte insertion changes at most 2 chunks;
- the streaming chunker matches the in-memory one;
- a 1-byte edit to a 10 MiB file stores at most 6 new objects and less
  than 1 MiB of new data.

The `fastcdc` crate (5.0.0) was evaluated and not adopted; see the
dependency records.

### Change detection

The index maps each path to its stat fingerprint `(size, mtime, ctime,
dev, ino, mode)` and the file object it produced.

- A matching fingerprint skips the read entirely.
- ctime cannot be set by user code, so back-dating mtime does not hide a
  change (`index_catches_backdated_and_replaced_files`).
- An entry whose mtime or ctime falls within 2 s of the recording scan is
  *racy* and is re-hashed once more on the next snapshot. This is the
  "racy-git" problem: a write in the same timestamp tick as the scan.
- The index is only a cache. It carries a SHA-256 trailer; a missing or
  corrupt index costs a full re-hash, and `gc` deletes it.

### Scan

The scan uses `anchor_fs::Workspace::scan`, a descriptor-relative walk:

- `openat(O_DIRECTORY|O_NOFOLLOW)` plus `fstatat(AT_SYMLINK_NOFOLLOW)` on
  Unix; path-based with re-verified directory handles on Windows;
- symlinks are recorded and never followed, so loops cannot hang it;
- limits on entries (default 1,000,000) and on depth (256) fail closed.

Content is read with `open_scanned`, which:

- re-opens strictly;
- refuses hardlinks;
- checks the file is the scanned version, and again after reading.

A file that races is re-examined once; if it races again, the snapshot
fails and the mutation is refused.

### Coverage

A snapshot covers everything in the workspace:

- **`.gitignore` is not honoured.** Ignored files such as `.env` are user
  data that commands can delete.
- **`.git/` is included**, so `git reset --hard` can be undone.

Exceptions:

- `[snapshot] exclude` in `workspace.toml` can leave paths out.
- **Untracked entries** are recorded by name and reason, but without
  content:
  - files with more than one hard link (restoring their content would turn
    outside data into an agent-readable regular file);
  - files over `max_file_mb` (default 256);
  - sockets and fifos;
  - over-long link targets.

  Restore never touches untracked entries. Snapshots report them, and the
  audit log records them.

### When snapshots are taken

The mediator takes one full-tree snapshot before the first mutation of
each turn (§5.5(a)). The mutating tools are write, edit and run_command. A
snapshot failure refuses the mutation.

### Atomic publication (§5.5(c))

1. New objects are staged in `tmp/` and made durable: one `syncfs` on
   Linux for batches over 64 objects, otherwise parallel per-file
   `fsync`.
2. They are renamed into `objects/`, and the fan-out directories are
   fsynced.
3. Only then is the ref written (temp + fsync + rename + directory fsync).

A kill at any point leaves either no new ref or a complete snapshot
(`kill_mid_snapshot_leaves_no_partial_snapshot`).

### Restore (§5.5(d))

1. **Snapshot the current state** (`pre`, kept as `restore/<ms>`, so a
   rollback can be undone).
2. **Pre-flight, read-only.** Hash-verify every object the forward pass
   writes and every pre-state object an undo would need. If the target is
   damaged, the restore is refused before anything is touched. If the
   pre-state is damaged, re-hash the workspace (which still holds that
   content) to heal it, then check again.
3. **Write the journal** `{pre, target}` durably.
4. **Apply the tree difference** as single, atomic, approval-gated
   `anchor-fs` operations:
   - exchange-and-verify writes with exact modes;
   - `create_dir`, `remove_dir`, `create_symlink`, `set_mode`;
   - removals are verified by content hash, and links by target, so a file
     changed by someone else meanwhile is never clobbered
     (`ConcurrentChange`).
5. **Verify** by rescanning: the workspace must equal the target, ignoring
   untracked entries.
6. **Move `HEAD`** and delete the journal.

If anything fails:

- **A failure during the restore** snapshots the current state (kept as
  `restore/<ms>-mid`, so nothing anyone wrote is lost) and applies the
  same machinery back to `pre`, verified.
- **A crash** leaves the journal behind. `anchor_backend::open_workspace`
  runs `recover()` before anything else and refuses to open if recovery
  fails.

Every pass is computed from the current state, so repeating an
interrupted pass is safe.

### Merge-queue substrate (§5.5(f))

- `diff(a, b)`;
- `merge3(base, ours, theirs)`: a path-level three-way merge. Identical
  or one-sided changes merge; directories merge recursively; differing
  changes to one path are conflicts that keep `ours`. There is no
  line-level merge yet.
- `materialize(snapshot, empty workspace)`, for populating worktrees.

Phase 4 builds the queue on these.

### Maintenance

- `verify()` (fsck): every object reachable from every ref or journal,
  including all chunks.
- `gc(keep_turns, keep_restores)`:
  - prunes old turn and restore refs (defaults 200 and 20);
  - marks everything reachable and sweeps the rest;
  - refuses to run on a damaged store.

  The mediator runs it automatically once turn refs exceed
  `keep_turns + 20`.

## Measured

See [`docs/benchmarks/snapshot.md`](../benchmarks/snapshot.md). On
100,000 files (571 MiB), incremental medians are:

| Environment | No change | 100 files changed | Target |
|---|---:|---:|---|
| Dev container (4 vCPU Xeon, ext4) | 0.77 s | 1.10 s | met |
| GitHub `ubuntu-latest` (2 vCPU EPYC, ext4; CI-gated at 2 s) | 0.40 s | 0.67 s | met |
| GitHub `macos-latest` (3 vCPU virtual M1, APFS) | 2.53 s | 2.59 s | **not met** |
| GitHub `windows-latest` (2 vCPU EPYC, NTFS, Defender on) | 3.95 s | 5.56 s | **not met** |

On macOS and Windows the per-file metadata scan dominates: 1.8–2.8 s on
macOS and 3.5–4.0 s on Windows. The next step is a parallel walk. After
that come large-fetch directory listings on Windows, and then
change-journal-driven scanning (FSEvents on macOS, the USN journal on
Windows).

## Consequences and limits

- **Restore memory.** Restore holds one file's content in memory at a time
  (a streaming write is future work). Files over `max_file_mb` are
  untracked.
- **Windows:**
  - symlinks are recorded but treated as untracked on restore (creating
    them needs elevation);
  - modes are not recorded;
  - the scan compares size and times only (the stable std API exposes no
    file index), and `open_scanned` reads the real identity.
- **No compression.** zstd would be a separately vetted dependency.
- **No migration** from the interim `.anchor/preimages` store (nothing had
  been released).
