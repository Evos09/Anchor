// Phase 6 done-when, end to end: a scripted two-model collaboration
// (examples/ui/team.toml) watched and steered in the dashboard only.
import { expect, test, type Page } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const bin = process.env.ANCHOR_BIN ?? join(repo, "target/debug/anchor");
const shots = join(repo, "ui/test-results/screens");

let proc: ChildProcess | null = null;
let dir = "";

test.afterAll(() => {
  proc?.kill("SIGTERM");
  if (dir) rmSync(dir, { recursive: true, force: true });
});

async function waitFor<T>(f: () => T | undefined, ms: number): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = f();
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function tab(page: Page, name: string) {
  await page.getByRole("tab", { name }).click();
}

test("a newcomer can follow and steer a two-model collaboration", async ({ page }) => {
  dir = mkdtempSync(join(tmpdir(), "anchor-e2e-"));
  const ws = join(dir, "ws");
  cpSync(join(repo, "examples/ui/workspace"), ws, { recursive: true });
  const urlFile = join(dir, "url");
  proc = spawn(
    bin,
    ["team", "run", "--workspace", ws, "--team", join(repo, "examples/ui/team.toml"), "--ui", "--ui-url-file", urlFile],
    { env: { ...process.env, ANCHOR_STATE_DIR: join(dir, "state") }, stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  proc.stdout?.on("data", (d) => (log += d));
  proc.stderr?.on("data", (d) => (log += d));
  const url = await waitFor(() => (existsSync(urlFile) ? readFileSync(urlFile, "utf8").trim() || undefined : undefined), 60_000);
  const token = url.split("#t=")[1]!;
  const api = async (path: string) =>
    (await page.request.get(new URL(path, url).toString(), { headers: { Authorization: `Bearer ${token}` } })).json();

  await page.goto(url);
  // The token leaves the address bar at once.
  await expect(page).not.toHaveURL(/#t=/);
  await expect(page.getByRole("heading", { name: "Agents" })).toBeVisible();

  // Answer every gate in the dashboard until the run is over.
  let approved = 0;
  let sawBlocked = false;
  const deadline = Date.now() + 180_000;
  while (Date.now() < deadline) {
    const ov = await api("/api/overview");
    if (!ov.running && ov.mode === "live" && ov.events > 0 && ov.pending.length === 0 && /run finished/.test(await page.locator(".top").innerText())) break;
    const button = page.getByRole("button", { name: "Approve" }).first();
    if (await button.isVisible().catch(() => false)) {
      if (!sawBlocked && (await page.getByTestId("approval-tool").count()) > 0) {
        await expect(page.locator(".status-blocked").first()).toBeVisible();
        sawBlocked = true;
        await page.screenshot({ path: join(shots, "01-approval-gate.png"), fullPage: true });
      }
      try {
        await button.click({ timeout: 3000 });
        approved++;
      } catch (e) {
        console.log("click retry:", String(e).split("\n")[0]);
      }
    } else {
      await page.waitForTimeout(150);
    }
  }
  expect(approved, log).toBeGreaterThanOrEqual(3);
  expect(sawBlocked).toBe(true);

  // The roster: both models, their families, done.
  for (const [name, family] of [
    ["alpha", "demo-family-a"],
    ["beta", "demo-family-b"],
  ] as const) {
    const card = page.getByTestId(`agent-${name}`);
    await expect(card).toContainText(family);
    await expect(card.getByTestId("status")).toHaveText("done");
  }

  // What happened, in order, in plain words (the "unfamiliar user" check).
  await tab(page, "What happened");
  const story = page.getByTestId("story");
  await expect(story).toContainText("Team started");
  const text = await story.innerText();
  const milestones = [
    "Team started",
    "alpha started a session",
    "alpha claimed calc.py",
    "alpha proposed a decision",
    "Merge #1 from alpha REJECTED (gate): test_failed",
    "alpha gets another attempt (2)",
    "Merge #2 passed verification (model reviewer",
    "Merge #2 from alpha applied",
    "beta started a session",
    "The Hub refused beta's request_claim",
    "Merge #3 from beta applied",
  ];
  let at = -1;
  for (const m of milestones) {
    const i = text.indexOf(m, at + 1);
    expect(i, `"${m}" after position ${at} in:\n${text}`).toBeGreaterThan(at);
    at = i;
  }
  await page.screenshot({ path: join(shots, "02-what-happened.png") });

  // The claim map: beta's refused claim on calc.py, in red.
  await tab(page, "Claim map");
  await expect(page.getByTestId("tree-calc.py")).toHaveAttribute("data-conflict", "true");
  await expect(page.getByTestId("conflict").first()).toContainText("calc.py");
  await page.screenshot({ path: join(shots, "03-claim-map.png"), fullPage: true });

  // The message stream: beta's report is held; release it.
  await tab(page, "Message stream");
  await expect(page.getByText("held for your approval")).toBeVisible();
  await page.screenshot({ path: join(shots, "04-message-stream.png"), fullPage: true });
  await page.getByRole("button", { name: "Release to alpha" }).click();
  await expect(page.getByText("released by you")).toBeVisible();

  // The merge queue: the rejection, with diff and evidence side by side.
  await tab(page, "Merge queue");
  await expect(page.getByTestId("merge-1")).toContainText("rejected");
  await page.getByTestId("merge-1").getByRole("button", { name: "diff & evidence" }).click();
  const ev = page.getByTestId("merge-evidence");
  await expect(ev).toContainText("test_failed");
  await expect(ev).toContainText("tests.test_clamp.C.test_high");
  await expect(ev.locator(".diff .add").first()).toBeVisible();
  await page.screenshot({ path: join(shots, "05-merge-evidence.png"), fullPage: true });

  // The decision log: accept alpha's decision.
  await tab(page, "Decision log");
  await expect(page.getByText("clamp() returns the nearest bound")).toBeVisible();
  await page.getByRole("button", { name: "Accept" }).click();
  await expect(page.locator(".decision .badge-ok")).toHaveText("accepted");
  await page.screenshot({ path: join(shots, "06-decision-log.png"), fullPage: true });

  // The cost meter: equal to the backend's exact sums, no discrepancies.
  await tab(page, "Cost meter");
  const cost = await api("/api/cost");
  await expect(page.getByTestId("cost-total")).toContainText(`$${cost.cost.total.spend_usd.toFixed(6)}`);
  for (const a of ["alpha", "beta", "reviewer"]) {
    await expect(page.getByTestId(`cost-agent-${a}`).getByTestId("spend")).toContainText(
      `$${cost.cost.per_agent[a].spend_usd.toFixed(6)}`,
    );
  }
  await expect(page.getByTestId("no-discrepancies")).toBeVisible();
  await page.screenshot({ path: join(shots, "07-cost-meter.png"), fullPage: true });

  // The snapshot timeline: restore the main workspace to before merge 2.
  await tab(page, "Snapshot timeline");
  await page.getByTestId("snapshot-merges/0000000002-pre").getByTestId("restore").click();
  await expect(page.getByRole("dialog")).toContainText("calc.py");
  await page.screenshot({ path: join(shots, "08-snapshot-timeline.png"), fullPage: true });
  await page.getByRole("dialog").getByRole("button", { name: "Restore" }).click();
  await expect(page.getByText(/Restored merges\/0000000002-pre/)).toBeVisible();
  expect(readFileSync(join(ws, "calc.py"), "utf8")).toBe("def clamp(x, lo, hi):\n    return x\n");

  // The slash commands.
  await page.getByLabel("Command").fill("/cost");
  await page.getByLabel("Command").press("Enter");
  await expect(page.getByRole("tab", { name: "Cost meter" })).toHaveAttribute("aria-selected", "true");

  // Everything is in the audit log, verified.
  await tab(page, "Audit log");
  await expect(page.getByText(/entries, each verified to extend the hash chain/)).toBeVisible();
});
