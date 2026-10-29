//! Unified Customizations Discovery Engine (Claude Code + Antigravity + DeepSeek Harness).
//!
//! Discovers rules (`ANCHOR.md`, `CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `.agents/rules/*.md`),
//! skills (`SKILL.md` with progressive disclosure), custom subagents, lifecycle hooks,
//! MCP server declarations, externalized `MEMORY.md` (with 20,000-token budget tracking),
//! and persisted session artifacts (`Plan`, `Report`, `Walkthrough`, `Diagram`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Token budget cap for always-on rules + `MEMORY.md` (DeepSeek Harness pattern).
pub const MEMORY_TOKEN_BUDGET: u64 = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredRule {
    pub name: String,
    pub scope: String,
    pub trigger: String,
    pub path: String,
    pub tokens_est: u64,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSkill {
    pub name: String,
    pub description: String,
    pub scope: String,
    pub path: String,
    pub has_scripts: bool,
    pub tokens_est: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSubagent {
    pub name: String,
    pub role: String,
    pub description: String,
    pub model_hint: String,
    pub tools: Vec<String>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredHook {
    pub event: String,
    pub matcher: String,
    pub command: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredMcpServer {
    pub name: String,
    pub transport: String,
    pub command_or_url: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStatus {
    pub path: String,
    pub exists: bool,
    pub used_tokens: u64,
    pub budget_tokens: u64,
    pub content_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub content: String,
    pub request_feedback: bool,
    pub agent: Option<String>,
    pub session_id: String,
    pub turn: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomizationsInventory {
    pub rules: Vec<DiscoveredRule>,
    pub skills: Vec<DiscoveredSkill>,
    pub subagents: Vec<DiscoveredSubagent>,
    pub hooks: Vec<DiscoveredHook>,
    pub mcp_servers: Vec<DiscoveredMcpServer>,
    pub memory: MemoryStatus,
    pub total_rule_tokens: u64,
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as u64) / 4).max(1)
}

/// Extract simple YAML frontmatter key-values (`--- ... ---`) from a Markdown file.
fn parse_frontmatter(text: &str) -> (std::collections::BTreeMap<String, String>, &str) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = text.trim_start_matches('\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n"))
        && let Some(end) = rest.find("\n---")
    {
        let fm = &rest[..end];
        for line in fm.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_ascii_lowercase();
                let v = v.trim().trim_matches('"').trim_matches('\'').to_string();
                if !k.is_empty() {
                    map.insert(k, v);
                }
            }
        }
        let body_start = end + "\n---".len();
        let body = rest.get(body_start..).unwrap_or("").trim_start();
        return (map, body);
    }
    (map, text)
}

/// Discover all rules, skills, subagents, hooks, MCP servers, and memory status
/// for `workspace_root` and `state_dir`. Read-only; never mutates the workspace.
pub fn discover(workspace_root: &Path, state_dir: &Path) -> CustomizationsInventory {
    let mut rules = Vec::new();
    let mut skills = Vec::new();
    let mut subagents = built_in_subagents();
    let mut hooks = Vec::new();
    let mut mcp_servers = Vec::new();

    // 1. Top-level instruction files (ANCHOR.md, CLAUDE.md, AGENTS.md, GEMINI.md).
    for file_name in [
        "ANCHOR.md",
        "CLAUDE.md",
        "AGENTS.md",
        "GEMINI.md",
        ".anchor/ANCHOR.md",
        ".claude/CLAUDE.md",
    ] {
        let candidate = workspace_root.join(file_name);
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            let (fm, body) = parse_frontmatter(&text);
            let trigger = fm
                .get("trigger")
                .cloned()
                .unwrap_or_else(|| "always_on".into());
            let preview: String = body.lines().take(4).collect::<Vec<_>>().join(" ");
            rules.push(DiscoveredRule {
                name: file_name.into(),
                scope: "workspace".into(),
                trigger,
                path: file_name.into(),
                tokens_est: estimate_tokens(&text),
                preview: preview.chars().take(160).collect(),
            });
        }
    }

    // 2. Scoped rule directories (.agents/rules, .claude/rules, .anchor/rules).
    for dir_rel in [".agents/rules", ".claude/rules", ".anchor/rules"] {
        let dir = workspace_root.join(dir_rel);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let p = ent.path();
                if p.extension().is_some_and(|e| e == "md")
                    && let Ok(text) = std::fs::read_to_string(&p)
                {
                    let (fm, body) = parse_frontmatter(&text);
                    let trigger = fm
                        .get("trigger")
                        .or_else(|| fm.get("globs"))
                        .cloned()
                        .unwrap_or_else(|| "always_on".into());
                    let name = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "rule".into());
                    let rel = p
                        .strip_prefix(workspace_root)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| name.clone());
                    let preview: String = body.lines().take(3).collect::<Vec<_>>().join(" ");
                    rules.push(DiscoveredRule {
                        name,
                        scope: "workspace".into(),
                        trigger,
                        path: rel,
                        tokens_est: estimate_tokens(&text),
                        preview: preview.chars().take(160).collect(),
                    });
                }
            }
        }
    }

    // 3. Skills (.agents/skills/*/SKILL.md, .claude/skills/*/SKILL.md, .anchor/skills/*/SKILL.md).
    for dir_rel in [".agents/skills", ".claude/skills", ".anchor/skills"] {
        let dir = workspace_root.join(dir_rel);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let skill_dir = ent.path();
                let skill_md = skill_dir.join("SKILL.md");
                if let Ok(text) = std::fs::read_to_string(&skill_md) {
                    let (fm, body) = parse_frontmatter(&text);
                    let default_name = skill_dir
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "skill".into());
                    let name = fm.get("name").cloned().unwrap_or(default_name);
                    let description = fm.get("description").cloned().unwrap_or_else(|| {
                        body.lines()
                            .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                            .unwrap_or("Workspace skill package")
                            .trim()
                            .chars()
                            .take(140)
                            .collect()
                    });
                    let rel = skill_md
                        .strip_prefix(workspace_root)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| name.clone());
                    skills.push(DiscoveredSkill {
                        name,
                        description,
                        scope: "workspace".into(),
                        path: rel,
                        has_scripts: skill_dir.join("scripts").is_dir(),
                        tokens_est: estimate_tokens(&text),
                    });
                }
            }
        }
    }

    // 4. Custom Subagents (.agents/agents/*.md, .claude/agents/*.md).
    for dir_rel in [".agents/agents", ".claude/agents", ".anchor/agents"] {
        let dir = workspace_root.join(dir_rel);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let p = ent.path();
                if p.extension().is_some_and(|e| e == "md")
                    && let Ok(text) = std::fs::read_to_string(&p)
                {
                    let (fm, body) = parse_frontmatter(&text);
                    let stem = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "agent".into());
                    let name = fm.get("name").cloned().unwrap_or(stem);
                    let description = fm.get("description").cloned().unwrap_or_else(|| {
                        body.lines()
                            .next()
                            .unwrap_or("Custom scoped subagent")
                            .chars()
                            .take(120)
                            .collect()
                    });
                    let model_hint = fm.get("model").cloned().unwrap_or_else(|| "inherit".into());
                    let rel = p
                        .strip_prefix(workspace_root)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_else(|_| name.clone());
                    subagents.push(DiscoveredSubagent {
                        name: name.clone(),
                        role: fm.get("role").cloned().unwrap_or(name),
                        description,
                        model_hint,
                        tools: vec!["read_file".into(), "glob".into(), "grep".into()],
                        source: rel,
                    });
                }
            }
        }
    }

    // 5. Lifecycle Hooks (.agents/hooks.json, .anchor/hooks.json).
    for hook_rel in [".agents/hooks.json", ".anchor/hooks.json", ".claude/hooks.json"] {
        let hp = workspace_root.join(hook_rel);
        if let Ok(text) = std::fs::read_to_string(&hp)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(obj) = val.get("hooks").and_then(|h| h.as_object())
        {
            for (event, list) in obj {
                if let Some(arr) = list.as_array() {
                    for item in arr {
                        let matcher = item
                            .get("matcher")
                            .and_then(|m| m.as_str())
                            .unwrap_or("*")
                            .to_string();
                        if let Some(inner) = item.get("hooks").and_then(|h| h.as_array()) {
                            for h in inner {
                                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                                    hooks.push(DiscoveredHook {
                                        event: event.clone(),
                                        matcher: matcher.clone(),
                                        command: cmd.to_string(),
                                        source: hook_rel.into(),
                                    });
                                }
                            }
                        } else if let Some(cmd) = item.get("command").and_then(|c| c.as_str()) {
                            hooks.push(DiscoveredHook {
                                event: event.clone(),
                                matcher,
                                command: cmd.to_string(),
                                source: hook_rel.into(),
                            });
                        }
                    }
                }
            }
        }
    }

    // 6. MCP Server Configurations (.agents/mcp_config.json, .mcp.json).
    for mcp_rel in [".agents/mcp_config.json", ".mcp.json", ".anchor/mcp_config.json"] {
        let mp = workspace_root.join(mcp_rel);
        if let Ok(text) = std::fs::read_to_string(&mp)
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(servers) = val
                .get("mcpServers")
                .or_else(|| val.get("servers"))
                .and_then(|s| s.as_object())
        {
            for (name, cfg) in servers {
                let (transport, target) = if let Some(url) = cfg.get("url").and_then(|u| u.as_str())
                {
                    ("http-sse".to_string(), url.to_string())
                } else if let Some(cmd) = cfg.get("command").and_then(|c| c.as_str()) {
                    ("stdio".to_string(), cmd.to_string())
                } else {
                    ("stdio".to_string(), "mcp-server".to_string())
                };
                mcp_servers.push(DiscoveredMcpServer {
                    name: name.clone(),
                    transport,
                    command_or_url: target,
                    source: mcp_rel.into(),
                });
            }
        }
    }

    // 7. Externalized Memory (MEMORY.md in state_dir/memory/MEMORY.md).
    let mem_path = state_dir.join("memory").join("MEMORY.md");
    let memory = match std::fs::read_to_string(&mem_path) {
        Ok(text) => MemoryStatus {
            path: mem_path.display().to_string(),
            exists: true,
            used_tokens: estimate_tokens(&text),
            budget_tokens: MEMORY_TOKEN_BUDGET,
            content_preview: text.chars().take(400).collect(),
        },
        Err(_) => MemoryStatus {
            path: mem_path.display().to_string(),
            exists: false,
            used_tokens: 0,
            budget_tokens: MEMORY_TOKEN_BUDGET,
            content_preview: String::new(),
        },
    };

    let total_rule_tokens: u64 = rules
        .iter()
        .map(|r| r.tokens_est)
        .sum::<u64>()
        .saturating_add(memory.used_tokens);

    CustomizationsInventory {
        rules,
        skills,
        subagents,
        hooks,
        mcp_servers,
        memory,
        total_rule_tokens,
    }
}

fn built_in_subagents() -> Vec<DiscoveredSubagent> {
    vec![
        DiscoveredSubagent {
            name: "research".into(),
            role: "Codebase & Architecture Researcher".into(),
            description: "Read-only subagent with glob, grep, read_file, and code_mode_batch for deep exploration without cluttering the parent context.".into(),
            model_hint: "planner".into(),
            tools: vec![
                "read_file".into(),
                "list_dir".into(),
                "glob".into(),
                "grep".into(),
                "code_mode_batch".into(),
            ],
            source: "built-in".into(),
        },
        DiscoveredSubagent {
            name: "implementer".into(),
            role: "Scoped Worker Agent".into(),
            description: "Isolated workspace executor with edit_file, multi_edit, apply_patch, and run_command.".into(),
            model_hint: "executor".into(),
            tools: vec![
                "read_file".into(),
                "write_file".into(),
                "edit_file".into(),
                "multi_edit".into(),
                "apply_patch".into(),
                "run_command".into(),
            ],
            source: "built-in".into(),
        },
        DiscoveredSubagent {
            name: "verifier".into(),
            role: "Cross-Family Empirical Reviewer".into(),
            description: "Independent model-family verifier that checks claims against diffs, test results, and line coverage.".into(),
            model_hint: "reviewer".into(),
            tools: vec!["submit_verdicts".into()],
            source: "built-in".into(),
        },
    ]
}

/// Read the full `SKILL.md` content for a skill by name (progressive disclosure).
pub fn load_skill_body(workspace_root: &Path, name: &str) -> Result<String, String> {
    for dir_rel in [".agents/skills", ".claude/skills", ".anchor/skills"] {
        let dir = workspace_root.join(dir_rel);
        let direct = dir.join(name).join("SKILL.md");
        if let Ok(text) = std::fs::read_to_string(&direct) {
            return Ok(text);
        }
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path().join("SKILL.md");
                if let Ok(text) = std::fs::read_to_string(&p) {
                    let (fm, _) = parse_frontmatter(&text);
                    if fm.get("name").is_some_and(|n| n == name) {
                        return Ok(text);
                    }
                }
            }
        }
    }
    Err(format!("skill {name:?} not found in .agents/skills, .claude/skills, or .anchor/skills"))
}

/// Persist an Antigravity-style artifact (`Plan`, `Report`, `Walkthrough`, `Diagram`)
/// in `<state_dir>/artifacts/` (outside the workspace so agent commands cannot tamper with it).
pub fn save_artifact(state_dir: &Path, artifact: &ArtifactRecord) -> std::io::Result<PathBuf> {
    let dir = state_dir.join("artifacts");
    std::fs::create_dir_all(&dir)?;
    let safe_id: String = artifact
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe_id}.json"));
    let json = serde_json::to_string_pretty(artifact).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// List all persisted artifacts in `<state_dir>/artifacts/`.
pub fn list_artifacts(state_dir: &Path) -> Vec<ArtifactRecord> {
    let dir = state_dir.join("artifacts");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for ent in entries {
        let p = ent.path();
        if p.extension().is_some_and(|e| e == "json")
            && let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(rec) = serde_json::from_str::<ArtifactRecord>(&text)
        {
            out.push(rec);
        }
    }
    out
}
