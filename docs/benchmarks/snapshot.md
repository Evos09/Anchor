# Snapshot benchmark (spec §14 Phase 3)

**Target:** a 100k-file monorepo snapshots incrementally in < 2 s.

**Harness:** `crates/anchor-snapshot/examples/bench.rs`.

```sh
cargo run --release -p anchor-snapshot --example bench -- --files 100000 --runs 5
```

## Workload

- 100,000 files in about 2,000 directories:
  `packages/pkgNN/src/modNN/fileNNNNNN.rs`.
- File sizes are log-normal with a median of 3 KiB. One file in 1,000 is
  1 MiB.
- One 32 MiB `.git` pack.
- 571 MiB in total. Content is pseudo-random, which is the worst case for
  dedup.

## Method

- **Measurements.** Each one is the median of 5 runs, taken after one
  warm-up snapshot. Time is wall-clock for `take_snapshot`, which
  includes:
  - the scan;
  - the index lookup;
  - reading, chunking and hashing changed files;
  - building trees;
  - durable publication (fsync / syncfs);
  - writing the ref.
- **Edits.** Changes are appends to random files. Between runs the
  harness waits 2.1 s, so each run starts outside the racy window.
- **Racy re-hashes.** "Re-hashed" counts include the previous run's edits.
  Their entries are racy (recorded within 2 s of their modification), so
  they are verified by content once more (ADR-0006). That is why "100
  files changed" re-hashes 200 files.
- **Restore** undoes 100 changed files. Its time includes the pre-restore
  snapshot, the pre-flight, the journal, 100 compare-and-swap writes and
  the verifying rescan.

## Results

### Development container (2026-10-06)

Environment:

- CPU: Intel Xeon @ 2.80 GHz, 4 logical CPUs;
- OS: Linux 6.18;
- filesystem: ext4 on a virtual disk shared with other tenants.

| Measurement | Median | Detail |
|---|---:|---|
| Initial snapshot (100,001 files) | 13–45 s | 584 MiB stored. I/O-bound; varied run to run on the shared disk |
| **Incremental, no change** | **771 ms** | scan 298 ms, 0 files re-hashed, 1 new object (the snapshot) |
| **Incremental, 100 files changed** | **1,100 ms** | scan 291 ms, 200 re-hashed, 377 new objects |
| Incremental, 1,000 files (1%) changed | 1,394 ms | scan 292 ms, 1,982 re-hashed, 2,860 new objects |
| Restore undoing 100 changed files | 2,244 ms | 100 file operations, plus the pre-restore snapshot and the verifying rescan |
| Verify whole store | 6,018 ms | 221,986 objects re-hashed |

Per-run times:

| Case | Runs (ms) |
|---|---|
| No change | 771, 713, 782, 737, 777 |
| 100 changed | 1100, 1174, 1211, 1068, 1068 |
| 1% changed | 1304, 1373, 1621, 1394, 1457 |

Breakdown of a no-change snapshot, from `ANCHOR_SNAPSHOT_PROFILE=1`:

| Phase | Time |
|---|---:|
| Descriptor-relative scan (100k `fstatat`) | ~300 ms |
| Index load (13 MiB, SHA-256 trailer) | ~100 ms |
| Collect | ~120 ms |
| Tree encoding and hashing (about 2,000 trees) | ~100 ms |
| Publish | ~100 ms |

**Result:** the target is met, with about 2.6× headroom with no changes
and 1.8× with 100 changed files.

### CI runners

The `snapshot-bench` job in `.github/workflows/ci.yml` runs on every
push. Its table is written to the job summary. On Linux it **fails** if
the no-change or 100-changed median exceeds 2 s. macOS and Windows runner
I/O varies too much for a hard bound, so they are reported only.

#### GitHub `ubuntu-latest`: run 37456108695 (gated)

Environment: AMD EPYC 9V45, 2 logical CPUs, Linux 6.17 (Azure), ext4.

| Measurement | Median | Detail |
|---|---:|---|
| Initial snapshot | 53,458 ms | 584 MiB stored |
| **Incremental, no change** | **401 ms** | scan 211 ms |
| **Incremental, 100 files changed** | **673 ms** | scan 208 ms, 200 re-hashed |
| Incremental, 1% changed | 1,495 ms | 1,982 re-hashed |
| Restore undoing 100 files | 1,330 ms | |
| Verify whole store | 3,063 ms | |

**PASS** under the 2 s gate. Runs:

- no change: 389, 401, 403, 394, 459 ms;
- 100 changed: 764, 673, 733, 662, 666 ms.

#### GitHub `macos-latest`: run 37456108695 (reported)

Environment: Apple M1 (virtual), 3 logical CPUs, APFS.

| Measurement | Median | Detail |
|---|---:|---|
| Initial snapshot | 152,951 ms | |
| Incremental, no change | **2,534 ms** | scan 1,788 ms |
| Incremental, 100 files changed | **2,587 ms** | scan 2,837 ms (in the median run) |
| Incremental, 1% changed | 5,324 ms | scan 4,856 ms |
| Restore undoing 100 files | 4,968 ms | |
| Verify whole store | 47,517 ms | |

**The target is not met on this runner.** Details:

- The scan alone takes 1.1–4.9 s for 100k `fstatat` calls. The other
  phases match Linux.
- The run is very noisy: no-change runs were 6,881, 755, 2,850, 2,534 and
  2,205 ms. That suggests a contended virtual disk; the fastest run met the
  target comfortably.
- This is a known macOS cost: per-file metadata calls on APFS are much
  slower than on ext4.

The fix is the same as the "next" item below: stop scanning. On macOS that
means FSEvents, which records changed directories since a given event id,
so only those directories are rescanned. A parallel directory walk is the
cheaper first step.

Until then, Anchor on macOS meets the 2 s target only for smaller
workspaces (cost is roughly linear in the file count, so about 50k files
on this runner).

#### GitHub `windows-latest`: run 37456108695 (reported)

Environment: AMD EPYC (family 25), 2 logical CPUs, Windows Server, NTFS
with Defender real-time scanning on.

| Measurement | Median | Detail |
|---|---:|---|
| Initial snapshot | 1,091,708 ms | 584 MiB stored, scan 3,651 ms |
| Incremental, no change | **3,954 ms** | scan 3,918 ms |
| Incremental, 100 files changed | **5,555 ms** | scan 3,539 ms, 200 re-hashed |
| Incremental, 1% changed | 11,434 ms | scan 4,006 ms, 1,982 re-hashed |
| Restore undoing 100 files | 14,195 ms | |
| Verify whole store | 207,497 ms | |

**The target is not met on this runner.** Details:

- No-change runs were 3,951, 3,886, 3,954, 3,961 and 4,480 ms. The scan
  is almost the whole cost: `read_dir` plus `symlink_metadata` per entry
  costs about 40 µs per file on NTFS, roughly 20× ext4.
- Every file write and open goes through Defender, so the phases that
  create files are also slow. Each new object is a new file with its own
  `fsync`, which is why the initial snapshot takes 18 minutes and restore
  takes 14 s.
- The whole job takes about 40 minutes, so its timeout is 75 minutes.

The planned fixes, in order:

1. The parallel walk described below.
2. `FindFirstFileExW` with `FIND_FIRST_EX_LARGE_FETCH`, which returns the
   size and times with the directory listing and so avoids a metadata
   call per file.
3. The USN change journal.

Packing small objects into pack files would cut the per-file cost of
initial snapshots and restores; it changes the store format and needs its
own ADR.

## Where the time goes next

- **The scan dominates on every platform**, and decides whether macOS
  meets the target. It could be parallelised across top-level directories
  (first step), then replaced by change journals that make the per-turn
  cost O(changes): fanotify on Linux, FSEvents on macOS, the USN journal
  on Windows. A journal can lose events, so a full scan stays as the
  fallback and as a periodic check.
- **The index** could be memory-mapped instead of parsed.
- **Initial snapshots** are I/O-bound (every byte is written once). It is
  a one-off cost per workspace.
