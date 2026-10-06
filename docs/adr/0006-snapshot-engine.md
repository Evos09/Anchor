# ADR-0006: Snapshot engine design (§17 Q6, §5.5)

- Status: Proposed (Phase 3)
- Date: 2026-10-06

## Decision (direction)

Build. The engine is the durability foundation and the substrate of the
merge queue; delegating it would put the core guarantee in someone else's
format.

- **Store**: content-addressed. Each blob is named by its SHA-256 and
  verified on read. Trees are stored as Merkle manifests.
- **Chunking**: content-defined chunking (FastCDC-class) for files above a
  threshold, so that edits store only new chunks. Candidate: `fastcdc`
  (crates.io 5.0.0, published 2026-08-22). It has no five-step record yet;
  implementing the published algorithm in-tree is the alternative.
- **Change detection**: reuse the previous snapshot's (path, size, mtime,
  ctime, inode) index to skip unchanged files without hashing. That is the
  only plausible route to the 100k-file < 2 s target.
- **Atomicity**: write new chunks and the manifest into a staging directory,
  fsync, then publish with a single rename of the snapshot ref. Restores
  write each file atomically through `anchor-fs` (approval-gated,
  descriptor-based). A restore journal makes an interrupted restore roll
  back to the pre-restore state (§5.5(d)).
- **Poisoning (Threat 9)**: blob hashes are verified on read, and manifest
  paths are confined through `Workspace::resolve` on restore. This is already
  done by the interim store (ADR-0018) and tested by
  `crafted_manifest_cannot_escape_on_restore`.

## Pending

Benchmarks on a generated 100k-file tree, published with hardware details.
If the target is missed, publish the measured number and the bottleneck.
