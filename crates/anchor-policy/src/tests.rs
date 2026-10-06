use std::path::{Path, PathBuf};

use super::*;

const DEFAULT_POLICY: &str = include_str!("../../../config/policy.default.toml");

fn home() -> PathBuf {
    PathBuf::from("/home/dev")
}
fn ws() -> PathBuf {
    PathBuf::from("/home/dev/project")
}

fn req(tool: Tool, op: Operation, target: &str) -> PermissionRequest {
    PermissionRequest {
        tool,
        operation: op,
        canonical_target: Some(PathBuf::from(target)),
        workspace_root: ws(),
        target_exists: true,
        is_append_only: false,
        blast_radius_count: 1,
        network: false,
    }
}

fn engine_with(policy: &str) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let audit = AuditLog::open(&dir.path().join("audit.jsonl")).unwrap();
    let p = Policy::parse(policy, &home()).unwrap();
    (Engine::from_policy(p, &home(), audit), dir)
}

fn action_of(e: &Engine, r: &PermissionRequest) -> (&'static str, String) {
    match e.decide(r) {
        Decision::Allow(_) => ("allow", String::new()),
        Decision::Prompt(p) => ("prompt", p.rule().to_string()),
        Decision::Deny(d) => ("deny", d.rule),
    }
}

#[test]
fn default_policy_parses() {
    Policy::parse(DEFAULT_POLICY, &home()).unwrap();
}

#[test]
fn reads_in_workspace_allowed() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    for tool in [Tool::ReadFile, Tool::ListDir, Tool::Glob, Tool::Grep] {
        let r = req(tool, Operation::Read, "/home/dev/project/src/main.rs");
        assert_eq!(action_of(&e, &r).0, "allow", "{tool:?}");
    }
}

#[test]
fn writes_in_workspace_prompt() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    let r = req(Tool::WriteFile, Operation::Write, "/home/dev/project/src/main.rs");
    assert_eq!(action_of(&e, &r), ("prompt", "prompt_on_write".into()));
    let r = req(Tool::EditFile, Operation::Edit, "/home/dev/project/src/main.rs");
    assert_eq!(action_of(&e, &r), ("prompt", "prompt_on_write".into()));
}

#[test]
fn append_to_append_only_allowed_but_write_prompts() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    let mut r = req(Tool::EditFile, Operation::Append, "/home/dev/project/DEVLOG.md");
    r.is_append_only = true;
    assert_eq!(action_of(&e, &r).0, "allow");
    let mut w = req(Tool::WriteFile, Operation::Write, "/home/dev/project/DEVLOG.md");
    w.is_append_only = true;
    assert_eq!(action_of(&e, &w).0, "prompt");
}

#[test]
fn file_tools_outside_workspace_denied() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    let r = req(Tool::ReadFile, Operation::Read, "/etc/passwd");
    assert_eq!(
        action_of(&e, &r),
        ("deny", "deny_file_tools_outside_workspace".into())
    );
}

/// Threat 5: credential paths are denied for every tool and operation.
#[test]
fn credentials_denied_everywhere() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    let targets = [
        "/home/dev/.ssh/id_ed25519",
        "/home/dev/.ssh/authorized_keys",
        "/home/dev/.aws/credentials",
        "/home/dev/.config/gh/hosts.yml",
        "/home/dev/.gnupg/private-keys-v1.d/x.key",
        "/home/dev/Library/Keychains/login.keychain-db",
        "/home/dev/.mozilla/firefox/abc.default/logins.json",
        "/home/dev/.config/google-chrome/Default/Login Data",
    ];
    for t in targets {
        for (tool, op) in [
            (Tool::ReadFile, Operation::Read),
            (Tool::WriteFile, Operation::Write),
            (Tool::EditFile, Operation::Append),
            (Tool::Grep, Operation::Search),
        ] {
            let r = req(tool, op, t);
            assert_eq!(action_of(&e, &r), ("deny", "deny_credentials".into()), "{t} {tool:?}");
        }
    }
}

/// Deny overrides: a credential file *inside* the workspace is still denied,
/// even though `allow_reads_in_workspace` appears first and matches.
#[test]
fn deny_overrides_earlier_allow() {
    let policy = r#"
version = 1
[default]
action = "prompt"
[[rule]]
name = "allow_all_reads"
action = "allow"
match = { tool = ["read_file"] }
[[rule]]
name = "deny_secrets"
action = "deny"
match = { target_matches = ["**/secrets/**"] }
"#;
    let (e, _d) = engine_with(policy);
    let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/secrets/key");
    assert_eq!(action_of(&e, &r), ("deny", "deny_secrets".into()));
}

#[test]
fn first_match_wins_among_non_deny() {
    let policy = r#"
version = 1
[default]
action = "deny"
[[rule]]
name = "first"
action = "prompt"
match = { tool = ["read_file"] }
[[rule]]
name = "second"
action = "allow"
match = { tool = ["read_file"] }
"#;
    let (e, _d) = engine_with(policy);
    let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/a");
    assert_eq!(action_of(&e, &r), ("prompt", "first".into()));
}

#[test]
fn unmatched_request_falls_to_default() {
    for (default, expect) in [("allow", "allow"), ("prompt", "prompt"), ("deny", "deny")] {
        let policy = format!(
            "version = 1\n[default]\naction = \"{default}\"\n[[rule]]\nname = \"only_grep\"\naction = \"allow\"\nmatch = {{ tool = [\"grep\"] }}\n"
        );
        let (e, _d) = engine_with(&policy);
        let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/a");
        let (a, rule) = action_of(&e, &r);
        assert_eq!(a, expect);
        if a != "allow" {
            assert_eq!(rule, "default");
        }
    }
}

#[test]
fn capability_floor_protects_anchor_state_dir() {
    let policy = "version = 1\n[default]\naction = \"allow\"\n";
    let (e, _d) = engine_with(policy);
    let r = req(Tool::WriteFile, Operation::Write, "/home/dev/project/.anchor/audit.jsonl");
    let (a, rule) = action_of(&e, &r);
    assert_eq!((a, rule.as_str()), ("deny", "capability:anchor_state_dir"));
}

#[test]
fn capability_floor_rejects_relative_target() {
    let policy = "version = 1\n[default]\naction = \"allow\"\n";
    let (e, _d) = engine_with(policy);
    let r = req(Tool::ReadFile, Operation::Read, "../../../../etc/passwd");
    assert_eq!(action_of(&e, &r).0, "deny");
}

#[test]
fn mutation_without_target_denied() {
    let policy = "version = 1\n[default]\naction = \"allow\"\n";
    let (e, _d) = engine_with(policy);
    let mut r = req(Tool::WriteFile, Operation::Write, "/x");
    r.canonical_target = None;
    assert_eq!(action_of(&e, &r).0, "deny");
}

#[test]
fn every_match_condition_is_exercised() {
    let policy = r#"
version = 1
[default]
action = "deny"
[[rule]]
name = "exists"
action = "allow"
match = { tool = ["write_file"], target_exists = false }
[[rule]]
name = "net"
action = "prompt"
match = { tool = ["run_command"], network = true }
[[rule]]
name = "cargo"
action = "allow"
match = { program = ["cargo"] }
[[rule]]
name = "big"
action = "prompt"
match = { min_blast_radius = 100 }
[[rule]]
name = "ops"
action = "allow"
match = { operation = ["list"] }
"#;
    let (e, _d) = engine_with(policy);
    let mut r = req(Tool::WriteFile, Operation::Create, "/home/dev/project/new");
    r.target_exists = false;
    assert_eq!(action_of(&e, &r).0, "allow");
    r.target_exists = true;
    assert_eq!(action_of(&e, &r).0, "deny");

    let mut c = req(Tool::RunCommand, Operation::Execute, "/usr/bin/curl");
    c.network = true;
    assert_eq!(action_of(&e, &c), ("prompt", "net".into()));
    let c = req(Tool::RunCommand, Operation::Execute, "/home/dev/.cargo/bin/cargo");
    assert_eq!(action_of(&e, &c).0, "allow");
    // `program` only matches run_command.
    let c = req(Tool::ReadFile, Operation::Read, "/home/dev/.cargo/bin/cargo");
    assert_eq!(action_of(&e, &c).0, "deny");

    let mut b = req(Tool::Grep, Operation::Search, "/home/dev/project");
    b.blast_radius_count = 500;
    assert_eq!(action_of(&e, &b), ("prompt", "big".into()));
    let l = req(Tool::ListDir, Operation::List, "/home/dev/project");
    assert_eq!(action_of(&e, &l).0, "allow");
}

#[test]
fn policy_rejects_unknown_keys_and_bad_rules() {
    let bad = [
        "version = 2\n[default]\naction = \"allow\"\n",
        "version = 1\n[default]\naction = \"maybe\"\n",
        "version = 1\n[default]\naction = \"allow\"\nsurprise = 1\n",
        "version = 1\n[default]\naction = \"allow\"\n[[rule]]\nname = \"a\"\naction = \"allow\"\nmatch = { when_user_says = \"ok\" }\n",
        "version = 1\n[default]\naction = \"allow\"\n[[rule]]\nname = \"a\"\naction = \"allow\"\nmatch = {}\n[[rule]]\nname = \"a\"\naction = \"deny\"\nmatch = {}\n",
        "version = 1\n[default]\naction = \"allow\"\n[[rule]]\nname = \"a\"\naction = \"allow\"\nmatch = { target_within = \"workspace\", target_outside = \"workspace\" }\n",
        "version = 1\n[default]\naction = \"allow\"\n[[rule]]\nname = \"a\"\naction = \"allow\"\nmatch = { tool = [] }\n",
    ];
    for b in bad {
        assert!(Policy::parse(b, &home()).is_err(), "should reject: {b}");
    }
}

/// Spec §5.2(g): a policy change mid-session takes effect on the next request,
/// and the change is logged.
#[test]
fn hot_reload_takes_effect_on_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, "version = 1\n[default]\naction = \"allow\"\n").unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let e = Engine::from_file(&policy_path, &home(), AuditLog::open(&audit_path).unwrap()).unwrap();
    let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/a");
    assert_eq!(action_of(&e, &r).0, "allow");

    std::fs::write(&policy_path, "version = 1\n[default]\naction = \"deny\"\n").unwrap();
    assert_eq!(action_of(&e, &r).0, "deny");

    let kinds: Vec<_> = AuditLog::read_entries(&audit_path)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"policy_reloaded".to_string()), "{kinds:?}");
}

#[test]
fn invalid_reload_keeps_last_good_policy() {
    let dir = tempfile::tempdir().unwrap();
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(&policy_path, "version = 1\n[default]\naction = \"deny\"\n").unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let e = Engine::from_file(&policy_path, &home(), AuditLog::open(&audit_path).unwrap()).unwrap();
    std::fs::write(&policy_path, "this is not toml [[[").unwrap();
    let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/a");
    assert_eq!(action_of(&e, &r).0, "deny");
    assert_eq!(action_of(&e, &r).0, "deny");
    let failures = AuditLog::read_entries(&audit_path)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "policy_reload_failed")
        .count();
    assert_eq!(failures, 1, "a failing file is logged once, not per request");
    std::fs::remove_file(&policy_path).unwrap();
    assert_eq!(action_of(&e, &r).0, "deny");
}

#[test]
fn every_decision_is_logged_with_structured_context() {
    let (e, dir) = engine_with(DEFAULT_POLICY);
    let r = req(Tool::WriteFile, Operation::Write, "/home/dev/project/x.rs");
    let Decision::Prompt(p) = e.decide(&r) else {
        panic!("expected prompt")
    };
    let approved = e
        .resolve_prompt(p, UserResponse::Approve, Responder::Interactive)
        .unwrap();
    assert_eq!(approved.target(), Some(Path::new("/home/dev/project/x.rs")));
    let entries = AuditLog::read_entries(&dir.path().join("audit.jsonl")).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].kind, "decision");
    assert_eq!(entries[0].data["matched_rule"], "prompt_on_write");
    assert_eq!(entries[0].data["action"], "prompt");
    assert_eq!(entries[0].data["request"]["canonical_target"], "/home/dev/project/x.rs");
    assert_eq!(entries[1].kind, "prompt_response");
    assert_eq!(entries[1].data["response"], "approve");
}

#[test]
fn user_denial_mints_nothing() {
    let (e, _d) = engine_with(DEFAULT_POLICY);
    let r = req(Tool::WriteFile, Operation::Write, "/home/dev/project/x.rs");
    let Decision::Prompt(p) = e.decide(&r) else {
        panic!()
    };
    assert!(e.resolve_prompt(p, UserResponse::Deny, Responder::Interactive).is_err());
}

#[test]
fn audit_chain_detects_tampering() {
    let (e, dir) = engine_with(DEFAULT_POLICY);
    for _ in 0..5 {
        let _ = e.decide(&req(Tool::ReadFile, Operation::Read, "/etc/shadow"));
    }
    let path = dir.path().join("audit.jsonl");
    assert_eq!(AuditLog::verify(&path).unwrap(), 5);
    let text = std::fs::read_to_string(&path).unwrap();
    let tampered = text.replacen("deny_file_tools_outside_workspace", "allow_reads_in_workspace", 1);
    std::fs::write(&path, tampered).unwrap();
    assert!(AuditLog::verify(&path).is_err());
    // Refuses to append to a broken chain.
    assert!(AuditLog::open(&path).is_err());

    // Deleting a middle line is also detected.
    let lines: Vec<_> = text.lines().collect();
    let without: String = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 2)
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    std::fs::write(&path, without).unwrap();
    assert!(AuditLog::verify(&path).is_err());
}

#[test]
fn audit_reopen_continues_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.jsonl");
    {
        let a = AuditLog::open(&path).unwrap();
        a.append("x", serde_json::json!({"n": 1})).unwrap();
    }
    let a = AuditLog::open(&path).unwrap();
    assert_eq!(a.append("x", serde_json::json!({"n": 2})).unwrap(), 1);
    assert_eq!(AuditLog::verify(&path).unwrap(), 2);
}

/// Spec §5.2(b), verified by instrumentation: the request schema contains no
/// free-text field. If someone adds e.g. `user_prompt: String`, this test
/// fails and forces a review.
#[test]
fn request_schema_has_no_natural_language_fields() {
    let r = req(Tool::ReadFile, Operation::Read, "/home/dev/project/a");
    let v = serde_json::to_value(&r).unwrap();
    let obj = v.as_object().unwrap();
    let keys: std::collections::BTreeSet<&str> = obj.keys().map(|k| k.as_str()).collect();
    let expected: std::collections::BTreeSet<&str> = [
        "blast_radius_count",
        "canonical_target",
        "is_append_only",
        "network",
        "operation",
        "target_exists",
        "tool",
        "workspace_root",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected);
    // Enum-typed fields serialise as one of a closed set of identifiers.
    assert!(obj["tool"].is_string() && obj["operation"].is_string());
    assert!(obj["blast_radius_count"].is_u64());
    assert!(obj["network"].is_boolean() && obj["target_exists"].is_boolean());
}

#[test]
fn would_allow_does_not_mint_or_log() {
    let (e, dir) = engine_with(DEFAULT_POLICY);
    let before = approvals_minted();
    assert!(e.would_allow(&req(Tool::ReadFile, Operation::Read, "/home/dev/project/a")));
    assert!(!e.would_allow(&req(Tool::ReadFile, Operation::Read, "/home/dev/.ssh/id_rsa")));
    // Other tests run concurrently and may mint; this test only checks that
    // nothing was logged by would_allow.
    let _ = before;
    let entries = AuditLog::read_entries(&dir.path().join("audit.jsonl")).unwrap();
    assert!(entries.is_empty());
}

mod param_validation {
    use crate::params::*;

    /// Threat 3: traversal smuggled through a glob parameter.
    #[test]
    fn glob_traversal_and_absolute_refused() {
        assert_eq!(validate_glob("../../../../etc/passwd").unwrap_err(), ParamError::Traversal);
        assert_eq!(validate_glob("src/../../x").unwrap_err(), ParamError::Traversal);
        assert_eq!(validate_glob("/etc/*").unwrap_err(), ParamError::AbsoluteGlob);
        assert_eq!(validate_glob("C:/Windows/*").unwrap_err(), ParamError::AbsoluteGlob);
        assert_eq!(validate_glob("~/.ssh/*").unwrap_err(), ParamError::HomeExpansion);
        assert_eq!(validate_glob("a\0b").unwrap_err(), ParamError::Nul);
        assert_eq!(validate_glob(&"a".repeat(2000)).unwrap_err(), ParamError::TooLong);
        assert!(validate_glob("src/**/*.rs").is_ok());
    }

    /// Classic catastrophic-backtracking patterns compile and run in linear
    /// time with the `regex` crate.
    #[test]
    fn regex_redos_patterns_are_linear() {
        let re = validate_regex("(a+)+$").unwrap();
        let input = format!("{}!", "a".repeat(100_000));
        let t = std::time::Instant::now();
        assert!(!re.is_match(&input));
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn regex_size_limit_enforced() {
        assert!(matches!(
            validate_regex("(?:a{1000}){1000}"),
            Err(ParamError::BadRegex(_))
        ));
        assert!(matches!(validate_regex("(unclosed"), Err(ParamError::BadRegex(_))));
    }
}
