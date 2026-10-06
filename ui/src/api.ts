// The dashboard's only channel to the backend. The access token arrives in
// the URL fragment (never sent to a server), is kept in sessionStorage for
// this tab, and goes out as a Bearer header; the fragment is then removed
// from the address bar and history.

const KEY = "anchor-dashboard-token";

function readToken(): string | null {
  const m = /(?:^#|&)t=([0-9a-f]{64})(?:&|$)/.exec(window.location.hash);
  if (m && m[1]) {
    try {
      sessionStorage.setItem(KEY, m[1]);
    } catch {
      /* storage unavailable: keep it in memory only */
    }
    history.replaceState(null, "", window.location.pathname + "#/story");
    return m[1];
  }
  try {
    return sessionStorage.getItem(KEY);
  } catch {
    return null;
  }
}

const token = readToken();

export function hasToken(): boolean {
  return token !== null;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function call<T>(method: "GET" | "POST", path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { Authorization: `Bearer ${token ?? ""}` };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const r = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
    credentials: "omit",
    referrerPolicy: "no-referrer",
  });
  const text = await r.text();
  let data: unknown = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = text;
  }
  if (!r.ok) {
    const msg =
      data && typeof data === "object" && "error" in data ? String((data as { error: unknown }).error) : `HTTP ${r.status}`;
    throw new ApiError(r.status, msg);
  }
  return data as T;
}

export const get = <T>(path: string) => call<T>("GET", path);
export const post = <T>(path: string, body: unknown) => call<T>("POST", path, body);

export function q(params: Record<string, string | number | undefined>): string {
  const u = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined) u.set(k, String(v));
  const s = u.toString();
  return s ? `?${s}` : "";
}
