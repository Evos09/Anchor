// The dashboard API's JSON shapes (crates/anchor-backend/src/ui).

export type Level = "trace" | "info" | "ok" | "warn" | "fail";

export interface Event {
  seq: number;
  ts_ms: number;
  kind: string;
  category: string;
  level: Level;
  agent: string | null;
  session_id: string | null;
  text: string;
  data: unknown;
}

export interface Tally {
  calls: number;
  tokens_in: number;
  tokens_out: number;
  reported_usd: number;
  computed_usd: number;
  unpriced_calls: number;
  spend_usd: number;
}

export type Status = "thinking" | "waiting" | "blocked" | "done" | "failed";

export interface RosterRow {
  name: string;
  agent_id: number | null;
  role: string;
  model_id: string;
  family: string;
  hub_status: string | null;
  status: Status;
  detail: string;
  instruction: string | null;
  claim_within: string[];
  session_id: string | null;
  sessions: number;
  cost: Tally;
}

export interface Pending {
  id: number;
  kind: "tool" | "merge" | "resume";
  agent: string | null;
  merge_seq: number | null;
  summary: string;
  tool: string | null;
  operation: string | null;
  target: string | null;
  rule: string | null;
  detail: string | null;
  choices: string[];
  created_ms: number;
}

export interface Overview {
  mode: "live" | "inspect" | "app";
  running: boolean;
  workspace: string;
  chain: { verified_entries: number; error: string | null };
  team: { schedule: string; agents: { name: string; instruction?: string }[]; verify: { kind: string } | null } | null;
  roster: RosterRow[];
  pending: Pending[];
  cost_total: Tally;
  cost_discrepancies: number;
  failures: number;
  events: number;
  last_seq: number | null;
  store: { available: boolean; note: string | null };
  rollback: string | null;
  hub: boolean;
}

export interface Claim {
  id: number;
  agent_id: number;
  agent: string | null;
  path: string;
  claim_type: string;
  isolated: boolean;
  lease_expires_at: number;
  status: string;
  created_at: number;
  lease_expired: boolean;
}

export interface Conflict {
  seq: number;
  agent: string | null;
  kind: "claim_refused" | "merge_conflict";
  path?: string;
  paths?: string[];
  merge?: number;
  reason: unknown;
}

export interface ClaimsView {
  claims: Claim[];
  files: string[];
  conflicts: Conflict[];
}

export interface Message {
  id: number;
  from: string;
  to: string;
  from_agent_id: number | null;
  to_agent_id: number | null;
  room_id: string | null;
  message_type: string;
  content: unknown;
  requires_approval: boolean;
  approved_by: string | null;
  state: "held" | "delivered" | "rejected";
  created_at: number;
}

export interface Decision {
  id: number;
  agent_id: number | null;
  agent: string | null;
  summary: string;
  rationale: string;
  evidence_ref: number | null;
  status: "proposed" | "accepted" | "rejected";
  decided_reason: string | null;
  created_at: number;
}

export interface Task {
  id: number;
  assigned_to_agent_id: number | null;
  description: string;
  status: string;
  created_at: number;
}

export interface Merge {
  seq: number;
  agent_id: number | null;
  agent: string | null;
  kind: string;
  status: "queued" | "applying" | "merged" | "rejected";
  feedback: Record<string, unknown>;
  reverts: number | null;
  created_at: number;
  expected_ms: number | null;
  evidence: string | null;
  findings: string[];
  caught_later: boolean;
}

export interface ChangedFile {
  path: string;
  status: string;
  is_test: boolean;
  diff: string;
}

export interface TestRecord {
  id: string;
  outcome: "passed" | "failed" | "error" | "skipped";
}

export interface CommandRecord {
  name: string;
  argv: string[];
  ok: boolean;
  exit: number | null;
  timed_out?: boolean;
  artifact?: string | null;
}

export interface ClaimText {
  id: string;
  text: string;
  files: string[];
  tests: string[];
}

export interface Finding {
  stage: string;
  code: string;
  detail: unknown;
}

export interface Verdict {
  claim_id: string;
  verdict: string;
  reason_code: string;
  note?: string;
}

export interface MergeDetail {
  merge: Merge;
  evidence_group: string | null;
  artifacts: { id: number; path: string; kind: string; sha256: string }[];
  decision: { approved: boolean; reviewer: string; findings: Finding[]; verdicts: Verdict[] } | null;
  claims: ClaimText[] | null;
  changes: ChangedFile[] | null;
  tests: TestRecord[] | null;
  commands: CommandRecord[] | null;
}

export interface CostLine {
  seq: number;
  agent: string;
  session_id: string | null;
  role: string;
  model_id: string;
  tokens_in: number;
  tokens_out: number;
  reported_usd: number;
  price: [number, number] | null;
  computed_usd: number | null;
}

export interface CostView {
  cost: {
    total: Tally;
    per_agent: Record<string, Tally>;
    per_session: Record<string, Tally>;
    per_model: Record<string, Tally>;
    lines: CostLine[];
    discrepancies: string[];
  };
  formula: string;
}

export interface Snapshot {
  ref: string;
  id: string;
  label: string;
  created_ms: number;
  files: number;
  bytes: number;
  root: string;
  kind: string;
  agent: string | null;
  session_id: string | null;
  turn: number | null;
  merge_seq: number | null;
  previous: string | null;
  changes: { path: string; kind: string }[];
  restorable: boolean;
}

export interface SnapshotsView {
  available: boolean;
  note?: string | null;
  snapshots: Snapshot[];
  rollback?: string | null;
}

// The desktop app (`anchor app`, ADR-0028).
export interface AppState {
  project: string | null;
  running: boolean;
  recent: string[];
  /** Recent folders that no longer exist. */
  missing: string[];
  models: {
    path: string;
    ok: boolean;
    error: string | null;
    roles: { executor: string | null; reviewer: string | null };
  };
  /** How tasks pick their models (Update #1). */
  inference?: {
    source: "routed" | "file";
    preference: Preference;
    pin: InfPath | null;
    free_offered: boolean;
    collab?: Collab;
    pair_rounds?: number;
    check_updates?: boolean;
  };
  mock: boolean;
  /** The desktop app's version; null when served to a browser. */
  version?: string | null;
  platform: string;
  home: string | null;
}

/** A: the free tier · B: your own key · C: local (Ollama). */
export type InfPath = "A" | "B" | "C";
export type Preference = "auto" | "prefer_local" | "prefer_byok" | "free_only";

export interface Meter {
  used: number;
  budget: number;
  resets_at_ms: number | null;
}

export interface InferenceStatus {
  source: "routed" | "file";
  preference: Preference;
  order: InfPath[] | null;
  pin: InfPath | null;
  preferred: Partial<Record<InfPath, string>>;
  models_file: string;
  local: {
    port: number;
    running: boolean;
    version: string | null;
    error: string | null;
    ready: {
      model: string;
      label: string;
      family: string;
      license: { id: string; commercial: boolean; gated: boolean; notes: string } | null;
    }[];
    needs_family: { model: string; hint: string; size: string }[];
    recommended: { model: string; label: string; family: string; role: string; size: number; license: string }[];
  };
  byok: {
    id: string;
    name: string;
    key_url: string;
    has_key: boolean;
    model: string | null;
    ready: boolean;
    problem: string | null;
  }[];
  free: {
    offered: boolean;
    started: boolean;
    state: { state: "available" } | { state: "exhausted" | "unavailable"; reason: string };
    usage: {
      now_ms: number;
      caps: { five_hour: Meter; weekly: Meter; monthly: Meter; blocked_by: string | null };
      pool: { providers: number; saturated: boolean };
    } | null;
  };
}

export interface PromptOption {
  path: InfPath;
  title: string;
  detail: string;
  ready: boolean;
}

/** What the app asks when no model can run a task. */
export interface TaskPrompt {
  reason: { kind: "nothing_available" | "path_unavailable"; path?: InfPath; why?: string };
  message: string;
  options: PromptOption[];
}

/** How many AIs work on a task (ADR-0039). */
export type Collab = "single" | "pair" | "team";

export interface TaskReply {
  ok: boolean;
  collab?: Collab;
  session_id?: string;
  prompt?: TaskPrompt;
  route?: {
    path: InfPath;
    executor: { model: string; family: string; path: InfPath };
    reviewer: { model: string; family: string; path: InfPath };
  } | null;
}

export interface DirListing {
  path: string | null;
  parent: string | null;
  dirs: string[];
  names?: string[];
  truncated?: boolean;
  home: string | null;
}

/** A model download through Ollama (`GET /api/app/pulls`). */
export interface PullState {
  model: string;
  status: string;
  total: number;
  completed: number;
  error: string | null;
  done: boolean;
}

/** The open project's test command (`GET /api/app/tests`). */
export interface TestsInfo {
  setup: { argv: string[]; timeout_secs: number } | null;
  suggested: string[] | null;
  why: string;
  declined: boolean;
}
