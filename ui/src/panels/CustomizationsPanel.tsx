import { useEffect, useState } from "react";
import { get } from "../api";
import type { CustomizationsResponse } from "../types";
import { Badge, Empty, ErrorLine, num } from "../util";

export function CustomizationsPanel({ version }: { version: number }) {
  const [data, setData] = useState<CustomizationsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    get<CustomizationsResponse>("/api/customizations").then(
      (res) => {
        setData(res);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version]);

  if (!data) return <ErrorLine error={error} />;
  const { inventory, policy_toml } = data;
  const memPct = Math.min(
    100,
    Math.round((inventory.memory.used_tokens / Math.max(1, inventory.memory.budget_tokens)) * 100),
  );

  return (
    <div className="wb-cust-grid">
      <ErrorLine error={error} />

      <div className="two">
        <div className="card">
          <div className="wb-section-head">
            <span>PERSISTENT MEMORY (MEMORY.MD)</span>
            <span className="mono small">
              {num(inventory.memory.used_tokens)} / {num(inventory.memory.budget_tokens)} tokens ({memPct}%)
            </span>
          </div>
          <div className="wb-meter" aria-label="Memory token budget">
            <div className="wb-meter-fill" style={{ width: `${memPct}%` }} />
          </div>
          {inventory.memory.exists ? (
            <>
              <div className="small muted mono" style={{ marginBottom: "0.4rem" }}>
                {inventory.memory.path}
              </div>
              <pre className="pre">{inventory.memory.content_preview || "(empty MEMORY.md)"}</pre>
            </>
          ) : (
            <Empty>
              No <code>MEMORY.md</code> found yet. Auto-memory persists project conventions within a 20,000-token ceiling.
            </Empty>
          )}
        </div>

        <div className="card">
          <div className="wb-section-head">
            <span>RULES HIERARCHY (ANCHOR.MD / CLAUDE.MD / AGENTS.MD)</span>
            <span className="mono">{inventory.rules.length}</span>
          </div>
          {!inventory.rules.length ? (
            <Empty>
              Drop <code>ANCHOR.md</code>, <code>CLAUDE.md</code>, <code>AGENTS.md</code>, or{" "}
              <code>.agents/rules/*.md</code> in the workspace to configure project rules.
            </Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>rule</th>
                  <th>scope</th>
                  <th>trigger</th>
                  <th>tokens</th>
                </tr>
              </thead>
              <tbody>
                {inventory.rules.map((r) => (
                  <tr key={r.path}>
                    <td>
                      <code>{r.name}</code>
                      <div className="small muted">{r.preview}</div>
                    </td>
                    <td>
                      <Badge kind="muted">{r.scope}</Badge>
                    </td>
                    <td>
                      <code>{r.trigger}</code>
                    </td>
                    <td className="mono">~{r.tokens_est}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      <div className="two" style={{ marginTop: "0.8rem" }}>
        <div className="card">
          <div className="wb-section-head">
            <span>PROGRESSIVE DISCLOSURE SKILLS (SKILL.MD)</span>
            <span className="mono">{inventory.skills.length}</span>
          </div>
          {!inventory.skills.length ? (
            <Empty>
              No skills discovered in <code>.agents/skills/*/SKILL.md</code> or <code>.claude/skills/*/SKILL.md</code>.
            </Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>skill</th>
                  <th>scope</th>
                  <th>summary</th>
                </tr>
              </thead>
              <tbody>
                {inventory.skills.map((s) => (
                  <tr key={s.path}>
                    <td>
                      <code>{s.name}</code>
                    </td>
                    <td>
                      <Badge kind="info">{s.scope}</Badge>
                    </td>
                    <td className="small">{s.description}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        <div className="card">
          <div className="wb-section-head">
            <span>SUBAGENT DEFINITIONS (ISOLATED WORKTREE / INHERIT)</span>
            <span className="mono">{inventory.subagents.length}</span>
          </div>
          <table className="table">
            <thead>
              <tr>
                <th>subagent</th>
                <th>source</th>
                <th>model</th>
                <th>role</th>
              </tr>
            </thead>
            <tbody>
              {inventory.subagents.map((a) => (
                <tr key={`${a.source}-${a.name}`}>
                  <td>
                    <code>{a.name}</code>
                  </td>
                  <td>
                    <Badge kind={a.source === "built-in" ? "muted" : "ok"}>{a.source}</Badge>
                  </td>
                  <td className="mono small">{a.model_hint}</td>
                  <td className="small">{a.description}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      <div className="two" style={{ marginTop: "0.8rem" }}>
        <div className="card">
          <div className="wb-section-head">
            <span>SANDBOXED LIFECYCLE HOOKS (PRE/POST TOOL USE)</span>
            <span className="mono">{inventory.hooks.length}</span>
          </div>
          {!inventory.hooks.length ? (
            <Empty>
              No lifecycle hooks configured in <code>.agents/hooks.json</code>, <code>.anchor/hooks.json</code>, or{" "}
              <code>.claude/hooks.json</code>.
            </Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>event</th>
                  <th>matcher</th>
                  <th>command</th>
                  <th>source</th>
                </tr>
              </thead>
              <tbody>
                {inventory.hooks.map((h, i) => (
                  <tr key={i}>
                    <td>
                      <Badge kind="warn">{h.event}</Badge>
                    </td>
                    <td>
                      <code>{h.matcher}</code>
                    </td>
                    <td>
                      <code>{h.command}</code>
                    </td>
                    <td className="mono small">{h.source}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>

        <div className="card">
          <div className="wb-section-head">
            <span>MCP SERVERS & CAPABILITY POLICY</span>
            <span className="mono">{inventory.mcp_servers.length} MCP</span>
          </div>
          {!inventory.mcp_servers.length ? (
            <Empty>
              No MCP servers declared in <code>.agents/mcp_config.json</code> or <code>.mcp.json</code>.
            </Empty>
          ) : (
            <table className="table">
              <thead>
                <tr>
                  <th>server</th>
                  <th>transport</th>
                  <th>target</th>
                </tr>
              </thead>
              <tbody>
                {inventory.mcp_servers.map((m) => (
                  <tr key={m.name}>
                    <td>
                      <code>{m.name}</code>
                    </td>
                    <td>
                      <Badge kind="info">{m.transport}</Badge>
                    </td>
                    <td>
                      <code>{m.command_or_url}</code>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {policy_toml && (
            <>
              <div className="wb-section-head" style={{ marginTop: "0.6rem" }}>
                <span>ACTIVE POLICY.TOML</span>
              </div>
              <pre className="pre">{policy_toml}</pre>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
