//! How the backend talks to the human. The CLI implementation prints to
//! stderr and reads answers from the controlling terminal; tests use a
//! scripted responder. Every prompt shows the canonical path only.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anchor_policy::{Operation, Responder, Tool, UserResponse};
use anchor_protocol::ResumeChoice;

/// What the user is asked to approve.
#[derive(Debug, Clone)]
pub struct PromptView {
    pub tool: Tool,
    pub operation: Operation,
    /// Canonical, fully resolved target. Never the model-supplied path.
    pub canonical_target: Option<PathBuf>,
    pub rule: String,
    /// Extra structured context (a deletion diff, an argv).
    pub detail: Option<String>,
    /// The agent the request is for (team runs).
    pub agent: Option<String>,
    /// Set for a merge approval.
    pub merge_seq: Option<i64>,
}

pub trait Interaction: Send {
    fn confirm(&mut self, view: &PromptView) -> UserResponse;
    fn resume_choice(&mut self, checkpoint_id: &str, ratio_hint: &str) -> ResumeChoice;
    fn notify(&mut self, line: &str);
    fn responder(&self) -> Responder;
    /// An independent interaction speaking for `name`, for implementations
    /// that can serve several agents at once (the dashboard). `None`: the
    /// caller serializes every agent through this one.
    fn split(&self, _name: &str) -> Option<Box<dyn Interaction>> {
        None
    }
}

/// Interactive terminal responder.
#[derive(Debug, Default)]
pub struct Terminal;

/// Text for the terminal with every control character made visible, so
/// model-chosen names, runtime output or file contents cannot move the
/// cursor, hide part of a prompt, retitle the window or write the clipboard
/// (escape sequences). Newlines are kept where `keep_newlines` is set.
pub fn terminal_safe(s: &str, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' if keep_newlines => out.push('\n'),
            '\t' => out.push('\t'),
            c if c.is_control()
                || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                out.extend(c.escape_default())
            }
            c => out.push(c),
        }
    }
    out
}

fn read_answer() -> String {
    // Prefer the controlling terminal so piped stdin cannot answer prompts.
    #[cfg(unix)]
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        // Discard anything typed (or injected) before the prompt appeared:
        // only an answer given after it counts.
        {
            use std::os::fd::AsRawFd;
            // SAFETY: tcflush on a descriptor we own.
            unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) };
        }
        let mut s = String::new();
        let _ = std::io::BufReader::new(tty).read_line(&mut s);
        return s.trim().to_ascii_lowercase();
    }
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_ascii_lowercase()
}

impl Interaction for Terminal {
    fn confirm(&mut self, v: &PromptView) -> UserResponse {
        let mut e = std::io::stderr().lock();
        let _ = writeln!(
            e,
            "\n── Permission required ({}) ──",
            terminal_safe(&v.rule, false)
        );
        let _ = writeln!(e, "  tool:      {:?}", v.tool);
        let _ = writeln!(e, "  operation: {:?}", v.operation);
        match &v.canonical_target {
            Some(t) => {
                let _ = writeln!(
                    e,
                    "  target:    {}",
                    terminal_safe(&t.display().to_string(), false)
                );
            }
            None => {
                let _ = writeln!(e, "  target:    (none)");
            }
        }
        if let Some(d) = &v.detail {
            let _ = writeln!(e, "{}", terminal_safe(d, true));
        }
        let _ = write!(e, "Allow? [y/N] ");
        let _ = e.flush();
        drop(e);
        if read_answer() == "y" {
            UserResponse::Approve
        } else {
            UserResponse::Deny
        }
    }

    fn resume_choice(&mut self, checkpoint_id: &str, ratio_hint: &str) -> ResumeChoice {
        eprintln!(
            "\n── Context window at {ratio_hint}: agent paused. Checkpoint {checkpoint_id} is on disk. ──\n  \
             [c] continue with compaction (checkpoint kept)\n  \
             [f] stop; start a fresh session seeded from the checkpoint (anchor run --resume {checkpoint_id})\n  \
             [r] stop; roll back to an earlier snapshot (anchor rollback)"
        );
        eprint!("Choice [c/f/r]: ");
        match read_answer().as_str() {
            "c" => ResumeChoice::ContinueWithCompaction,
            "r" => ResumeChoice::Rollback,
            _ => ResumeChoice::FreshSession,
        }
    }

    fn notify(&mut self, line: &str) {
        eprintln!("{}", terminal_safe(line, true));
    }

    fn responder(&self) -> Responder {
        Responder::Interactive
    }
}

/// Non-interactive responder selected explicitly on the command line. Its
/// answers are recorded in the audit log as `automatic`.
#[derive(Debug)]
pub struct Scripted {
    pub approve: bool,
    pub resume: ResumeChoice,
    pub prompts_seen: Vec<PromptView>,
    pub notices: Vec<String>,
    pub echo: bool,
}

impl Scripted {
    pub fn new(approve: bool, resume: ResumeChoice) -> Self {
        Scripted {
            approve,
            resume,
            prompts_seen: Vec::new(),
            notices: Vec::new(),
            echo: false,
        }
    }
}

impl Interaction for Scripted {
    fn confirm(&mut self, v: &PromptView) -> UserResponse {
        if self.echo {
            eprintln!(
                "[auto-{}] {:?} {:?} {} (rule {})",
                if self.approve { "approve" } else { "deny" },
                v.tool,
                v.operation,
                terminal_safe(
                    &v.canonical_target
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    false
                ),
                terminal_safe(&v.rule, false)
            );
        }
        self.prompts_seen.push(v.clone());
        if self.approve {
            UserResponse::Approve
        } else {
            UserResponse::Deny
        }
    }
    fn resume_choice(&mut self, _checkpoint_id: &str, _ratio_hint: &str) -> ResumeChoice {
        self.resume
    }
    fn notify(&mut self, line: &str) {
        if self.echo {
            eprintln!("{}", terminal_safe(line, true));
        }
        self.notices.push(line.to_string());
    }
    fn responder(&self) -> Responder {
        Responder::Automatic
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::terminal_safe;

    /// Review finding T-3: escape sequences in a model-chosen name or in
    /// runtime output are shown, never interpreted by the terminal.
    #[test]
    fn control_characters_are_made_visible() {
        let hostile = "notes\u{1b}[8m\u{1b}]52;c;cHduZWQ=\u{7}.txt\r\u{202e}gpj.exe";
        let shown = terminal_safe(hostile, false);
        assert!(!shown.chars().any(|c| c.is_control()), "{shown}");
        assert!(shown.contains("\\u{1b}[8m"), "{shown}");
        assert!(shown.contains("\\u{202e}"), "{shown}");
        assert_eq!(terminal_safe("a\nb", true), "a\nb");
        assert_eq!(terminal_safe("a\nb", false), "a\\nb");
    }
}
