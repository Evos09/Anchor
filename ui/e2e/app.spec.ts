// Phase 9, end to end: the desktop app's page (`anchor app`, ADR-0028)
// with a scripted model (examples/app): open the folder in the picker,
// send a task, approve its two changes, see the answer, undo the turn.
import { expect, test } from "@playwright/test";
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

test("open a folder, run a task, approve, undo", async ({ page }) => {
  dir = mkdtempSync(join(tmpdir(), "anchor-app-e2e-"));
  const ws = join(dir, "my-project");
  cpSync(join(repo, "examples/app/workspace"), ws, { recursive: true });
  const urlFile = join(dir, "url");
  const app = join(repo, "examples/app");
  proc = spawn(
    bin,
    ["app", "--roles", join(app, "roles.mock.toml"), "--mock-script", join(app, "script.json"), "--ui-url-file", urlFile],
    { env: { ...process.env, ANCHOR_STATE_DIR: join(dir, "state") }, stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  proc.stdout?.on("data", (d) => (log += d));
  proc.stderr?.on("data", (d) => (log += d));
  const url = await waitFor(() => (existsSync(urlFile) ? readFileSync(urlFile, "utf8").trim() || undefined : undefined), 60_000);

  await page.goto(url);
  await expect(page).not.toHaveURL(/#t=/);
  await expect(page.getByRole("heading", { name: "Open a project to start" })).toBeVisible();
  await page.screenshot({ path: join(shots, "app-01-welcome.png"), fullPage: true });

  // The folder picker: type the parent, then pick the project by name.
  await page.getByRole("button", { name: "Open folder…" }).first().click();
  const picker = page.getByRole("dialog", { name: "Open a folder" });
  await picker.getByLabel("Folder path").fill(dir);
  await picker.getByRole("button", { name: "Go", exact: true }).click();
  await picker.getByRole("button", { name: /my-project/ }).click();
  await picker.getByTestId("open-here").click();
  await expect(page.getByTestId("project")).toContainText("my-project");

  await page.getByLabel("Task").fill("Add a README and make the greeting mention anchor");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByTestId("task")).toContainText("Add a README");

  // Two approvals: the edit, then the new file.
  for (const target of ["hello.py", "README.md"]) {
    const card = page.getByTestId("approval-tool");
    await expect(card).toContainText(target, { timeout: 60_000 });
    if (target === "hello.py") await page.screenshot({ path: join(shots, "app-02-approval.png"), fullPage: true });
    await card.getByRole("button", { name: "Approve" }).click();
    await expect(card.filter({ hasText: target })).toHaveCount(0, { timeout: 30_000 });
  }
  await expect(page.getByTestId("result")).toHaveText("Added a README and made the greeting mention anchor.", {
    timeout: 60_000,
  });
  await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  expect(readFileSync(join(ws, "hello.py"), "utf8")).toContain("hello, anchor");
  expect(existsSync(join(ws, "README.md"))).toBe(true);
  await page.screenshot({ path: join(shots, "app-03-done.png"), fullPage: true });

  // Undo the turn that made the changes; the preview names both files.
  await page.getByTestId("undo-2").click();
  const confirm = page.getByRole("dialog", { name: "Confirm undo" });
  await expect(confirm).toContainText("hello.py");
  await expect(confirm).toContainText("README.md");
  await confirm.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "Undone" })).toBeVisible();
  expect(readFileSync(join(ws, "hello.py"), "utf8")).toBe("print('hello')\n");
  expect(existsSync(join(ws, "README.md"))).toBe(false);
  await page.screenshot({ path: join(shots, "app-04-undone.png"), fullPage: true });
  expect(log).not.toMatch(/panicked/);
});
