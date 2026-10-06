import type { ReactNode } from "react";

/** A stable color per agent, in roster order. */
const PALETTE = ["#2563eb", "#d97706", "#059669", "#9333ea", "#db2777", "#0891b2", "#65a30d", "#dc2626"];

export function makeColors(names: string[]): (name: string | null | undefined) => string {
  const map = new Map<string, string>();
  names.forEach((n, i) => map.set(n, PALETTE[i % PALETTE.length] ?? "#64748b"));
  return (n) => (n ? (map.get(n) ?? "#64748b") : "#64748b");
}

export function usd(n: number | null | undefined, digits = 4): string {
  if (n === null || n === undefined) return "—";
  return `$${n.toFixed(digits)}`;
}

export function num(n: number): string {
  return n.toLocaleString("en-US");
}

export function time(ms: number): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

export function dateTime(ms: number): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString();
}

export function duration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "—";
  if (ms < 1000) return `${ms} ms`;
  const s = Math.round(ms / 1000);
  return s < 120 ? `${s} s` : `${Math.round(s / 60)} min`;
}

/** Text for any JSON value: strings as they are, the rest pretty-printed. */
export function show(v: unknown): string {
  if (typeof v === "string") return v;
  if (v === null || v === undefined) return "";
  return JSON.stringify(v, null, 2);
}

export function AgentChip({ name, color }: { name: string | null | undefined; color: string }) {
  return (
    <span className="chip" style={{ borderColor: color, color }}>
      <span className="dot" style={{ background: color }} aria-hidden="true" />
      {name ?? "backend"}
    </span>
  );
}

export function Badge({ kind, children }: { kind: string; children: ReactNode }) {
  return <span className={`badge badge-${kind}`}>{children}</span>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="empty">{children}</p>;
}

export function ErrorLine({ error }: { error: string | null }) {
  if (!error) return null;
  return (
    <p className="error" role="alert">
      {error}
    </p>
  );
}
