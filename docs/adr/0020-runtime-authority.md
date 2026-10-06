# ADR-0020: Enforcing "the runtime has no OS authority" (constraint 3)

- Status: Accepted
- Date: 2026-10-06

Three layers:

1. **Process boundary**: `anchor-agent` is a separate process, spawned with
   a cleared environment (only proxy/TLS variables and the API-key variables
   named in the roles config), cwd set to the temp dir, and stdio pipes.
2. **Static check**: `crates/anchor-runtime/clippy.toml` disallows
   `std::fs::*` and `std::process::Command`. CI runs clippy with
   `-D warnings`. The only exception is the explicitly allowed self-test
   that *proves* the sandbox works.
3. **Kernel enforcement** (Linux): Landlock self-restriction before the
   first frame is read (ADR-0003). The test
   `runtime_process_cannot_touch_the_filesystem` shows reads and writes of
   workspace files fail.

Instrumentation for "no filesystem mutation bypasses the permission
engine":

- Mutating `anchor-fs` functions require an `Approved` value whose
  constructor is private to `anchor-policy`.
- `anchor_fs::mutations_performed()` must equal the number of audited
  `mutation` entries, and every mutation's decision id must have an
  approving decision or prompt response
  (`end_to_end_session_enforces_every_primitive`).
