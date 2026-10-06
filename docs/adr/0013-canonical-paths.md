# ADR-0013: Canonical path resolution and TOCTOU design (§5.1)

- Status: Accepted (Linux, tested); macOS and Windows implemented and
  type-checked, not yet executed against the attack suite
- Date: 2026-10-06

## Decision

`anchor-fs` (see the crate docs for the guarantees):

1. **Lexical confinement**: `..` may not climb above the root. An absolute
   parameter must lie under the canonical root (then it is confined) or is
   refused. NUL and empty strings are refused.
2. **Kernel resolution against a root handle**:
   - Linux: `openat2(O_PATH, RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`.
     Symlinks inside the workspace are followed; any escape returns `EXDEV`.
   - macOS: `openat`, then `fcntl(F_GETPATH)`, comparing against the root.
   - Windows: `CreateFileW`, then `GetFinalPathNameByHandleW`.
3. **Canonical path from the handle**: `/proc/self/fd/N`, `F_GETPATH`, or
   `GetFinalPathNameByHandleW`. Only this path is ever shown to the user or
   given to the policy engine.
4. **No re-resolution**: later operations open the canonical, symlink-free
   relative path with symlinks forbidden in every component
   (`RESOLVE_NO_SYMLINKS` / `O_NOFOLLOW_ANY` / `FILE_FLAG_OPEN_REPARSE_POINT`
   plus a final-path check). They also verify the descriptor's real path.
5. **Atomic replace**: write a temp file in the same directory fd, fsync,
   then `renameat2(RENAME_EXCHANGE)` (macOS `RENAME_SWAP`, Windows
   `ReplaceFileW` with backup). The swap is reversed if the displaced file
   is not the approved one. If the filesystem lacks exchange support, the
   write is refused rather than done non-atomically.
6. **Identity** is `(dev, inode)` plus ctime. ctime was added after a test
   showed ext4 reusing an inode number immediately after delete+create.
   Hardlinked regular files (`nlink > 1`) are refused for reads and writes.
7. **Approval binding**: every mutation consumes an `Approved` token whose
   canonical target and operation must match.

### Crate choice: `rustix` (vs `nix`, raw `libc`)

`rustix` exposes `openat2` + `ResolveFlags`, `getpath` (macOS),
`renameat_with` (EXCHANGE / RENAME_SWAP) and `O_NOFOLLOW_ANY` through safe
APIs. With raw `libc` this would all be `unsafe`. `nix` was not needed once
rustix covered the surface.

## Interpretations (spec ambiguities)

- §5.1(d) offers `RESOLVE_NO_SYMLINKS` for Linux. Applying it at resolution
  time would make every in-workspace symlink unusable. We apply
  `RESOLVE_BENEATH` (follow, but never escape) at resolution and
  `RESOLVE_NO_SYMLINKS` for every subsequent operation, which closes the
  swap race (`symlink_swap_race_never_escapes`,
  `parent_swapped_after_resolution_is_refused`).
- `..` is normalised lexically before kernel resolution. This differs from
  POSIX semantics when `a/..` crosses a symlink. The lexical result is
  always at least as confined.
- "Hardlink inside the workspace to a file outside": a link count cannot
  tell where the other names live, so all multiply-linked regular files are
  refused. Workspaces using hardlink-based package stores (e.g. pnpm) need
  an explicit opt-in (`allow_hardlinked_reads`, not yet exposed).

## Known gaps

- Windows `list_dir` enumerates by path (`read_dir`) between two
  handle-verified checks. Moving to `GetFileInformationByHandleEx` is a
  Phase 7 item.
- Windows identity uses last-write time where Unix uses ctime.
- macOS case/Unicode equivalence uses NFC + lowercase as an approximation
  of APFS rules. The authority is the kernel's `F_GETPATH` result.

## Verification

`cargo test -p anchor-fs`: 23 tests, including every §5.1 test bullet that
applies on Linux. The case and NFC/NFD tests are `cfg`-gated to macOS and
Windows and run only in CI there.
