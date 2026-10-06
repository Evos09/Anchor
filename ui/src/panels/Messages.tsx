import { useEffect, useState } from "react";
import { get, post } from "../api";
import type { Message } from "../types";
import { AgentChip, Badge, Empty, ErrorLine, show, time } from "../util";

/** The message stream (§12.2): every inter-agent message, held ones
 * highlighted with their approval gate. Merge feedback (review messages)
 * links to its diff and evidence. */
export function Messages({
  version,
  colors,
  onMerge,
}: {
  version: number;
  colors: (n: string | null | undefined) => string;
  onMerge: (seq: number) => void;
}) {
  const [msgs, setMsgs] = useState<Message[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    get<Message[]>("/api/messages").then(
      (m) => {
        setMsgs(m);
        setError(null);
      },
      (e) => setError(String(e)),
    );
  }, [version, tick]);
  const decide = async (id: number, approve: boolean) => {
    try {
      await post("/api/messages/decide", { id, approve });
      setError(null);
    } catch (e) {
      setError(String(e));
    }
    setTick((t) => t + 1);
  };
  return (
    <div>
      <ErrorLine error={error} />
      {!msgs.length && <Empty>No messages yet.</Empty>}
      <ol className="messages">
        {msgs.map((m) => {
          const c = m.content as { text?: unknown; merge?: unknown; status?: unknown };
          const merge = typeof c?.merge === "number" ? c.merge : null;
          return (
            <li key={m.id} className={`msg msg-${m.state}`} data-testid={`message-${m.id}`}>
              <div className="row">
                <AgentChip name={m.from} color={colors(m.from)} />
                <span aria-hidden="true">→</span>
                <AgentChip name={m.to} color={colors(m.to)} />
                <Badge kind="muted">{m.message_type}</Badge>
                {m.state === "held" && <Badge kind="warn">held for your approval</Badge>}
                {m.state === "rejected" && <Badge kind="fail">rejected by you</Badge>}
                {m.approved_by && m.state === "delivered" && <Badge kind="ok">released by you</Badge>}
                <span className="muted small">{time(m.created_at)}</span>
              </div>
              <pre className="pre msg-body">{typeof c?.text === "string" ? c.text : show(m.content)}</pre>
              {merge !== null && (
                <button className="link" onClick={() => onMerge(merge)}>
                  Diff and evidence for merge #{merge}
                </button>
              )}
              {m.state === "held" && (
                <div className="actions">
                  <button className="btn btn-primary" onClick={() => decide(m.id, true)}>
                    Release to {m.to}
                  </button>
                  <button className="btn" onClick={() => decide(m.id, false)}>
                    Reject
                  </button>
                </div>
              )}
            </li>
          );
        })}
      </ol>
    </div>
  );
}
