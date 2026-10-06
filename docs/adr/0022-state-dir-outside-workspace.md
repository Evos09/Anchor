# ADR-0022: Backend state lives outside the workspace

- Status: Accepted
- Date: 2026-10-06

## Context

Until now, all per-workspace backend state lived in `<workspace>/.anchor/`:

- the policy (`policy.toml`, hot-reloaded);
- the hash-chained audit log;
- checkpoints;
- `workspace.toml`;
- the durability store.

The capability floor (ADR-0011) denies the agent's *file tools* any target
under `.anchor/`. But approved agent commands run with Landlock / Seatbelt
read-write access to the **whole workspace root** (ADR-0003). A sandbox rule
cannot carve a subtree out of a granted path. So an approved command such as
`python -c "open('.anchor/policy.toml','w').write(...)"` could have:

- rewritten the policy, which takes effect on the next request through hot
  reload;
- truncated the audit log;
- poisoned the snapshot store (Threat 9, §5.5(e)).

This hole was found while planning Phase 3.

## Decision

State moves to a per-workspace directory **outside** the workspace:

| Platform | Base |
|---|---|
| any | `$ANCHOR_STATE_DIR` if set |
| Linux / BSD | `$XDG_STATE_HOME/anchor`, else `~/.local/state/anchor` |
| macOS | `~/Library/Application Support/anchor` |
| Windows | `%LOCALAPPDATA%\anchor` |

- **Directory**: each workspace's state is
  `<base>/ws/<first 16 hex of sha256(canonical workspace root)>/`. It holds
  a `workspace-root` file naming the workspace, for humans.
- **Permissions**: on Unix the directory is created with mode `0700`, and
  any looser mode is tightened.
- **Overlap**: opening is **refused** if the state directory and the
  workspace overlap (for example, a workspace of `~` with the default
  base). The error tells the user to set `ANCHOR_STATE_DIR`.
- **Sandboxes**: commands are granted only the workspace (read-write) and
  system/toolchain directories (read-only). They get no access to the state
  directory, so Landlock and Seatbelt deny both reads and writes. When the
  backend runs as root, commands also run as uid 65534, which the `0700`
  mode excludes.
- **Engine floor**: the engine gains `Engine::protect(dir)`. Every request
  targeting the state directory is floor-denied for every tool, including
  user-initiated actions. This is defence in depth: file tools are already
  confined to the workspace by `anchor-fs`. The `.anchor/` floor rule
  remains.
- **CLI**: `anchor state-dir --workspace DIR` prints the location.

## Consequences

- There is no migration from `.anchor/`: nothing has been released. If an
  old `.anchor/` directory exists, opening the workspace prints a one-line
  notice and ignores it.
- Moving or renaming a workspace gives it a new state directory, because the
  id derives from the canonical root. A later `anchor state-dir --adopt`
  could re-link state if users need it.
- **Test**: `command_cannot_touch_state_dir` (e2e) runs approved commands
  that try to overwrite the policy, create a file in the state directory and
  read the audit log. All three fail, the policy bytes are unchanged, and
  the file-tool attempts are refused.
