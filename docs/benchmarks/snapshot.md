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

(Runner results are recorded below once the job has run.)

## Where the time goes next

- **The scan dominates.** It could be parallelised across top-level
  directories, or replaced on Linux by fanotify / inotify change journals.
  That would turn the per-turn cost into O(changes).
- **The index** could be memory-mapped instead of parsed.
- **Initial snapshots** are I/O-bound (every byte is written once). It is
  a one-off cost per workspace.
