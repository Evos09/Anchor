# Security

Anchor's product is trustworthiness, so security reports are the most
valuable contribution you can make.

## Reporting

Please report vulnerabilities privately through GitHub Security Advisories
on this repository ("Report a vulnerability"). Do not open public issues for
security bugs. Include a reproducer if you can: the attack-suite tests in
`crates/anchor-fs/src/tests.rs` and `crates/anchor-backend/tests/e2e.rs` are
good templates.

## Scope

In scope: anything that lets an agent (or a model, a tool parameter, a file
in the workspace, or a crafted checkpoint or manifest) do any of these:

- read, write or execute outside the workspace;
- bypass the permission engine or forge an approval;
- modify an append-only file other than by appending;
- overwrite a file it has not read without the user's confirmation;
- reach the network from a command without approval;
- corrupt or escape the durability stores or the audit log without
  detection;
- run a verifier plugin outside its sandbox, or have a plugin's answer
  approve a change the built-in reviewer rejects (`docs/verifier/`).

The current per-platform status of each mitigation is in
[`docs/threat-model.md`](docs/threat-model.md). The risks we accept, with
their reasons, are listed in
[`docs/audit/accepted-risks.md`](docs/audit/accepted-risks.md). Reports that
demonstrate them in new ways are still welcome.

## External audit

An external audit (§11.2) is required before v1.0 and has not happened
yet. The package prepared for the auditor is
[`docs/audit/README.md`](docs/audit/README.md). No release is cut before the
audit is done and its Critical and High findings are fixed.
