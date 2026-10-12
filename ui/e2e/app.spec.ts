// Phase 9, end to end: the desktop app's page (`anchor app`, ADR-0028)
// with a scripted model (examples/app): open the folder in the picker,
// send a task, approve its two changes, see the answer, undo the turn,
// find the task in the sidebar's history, check the models in Settings.
import { expect, test } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { createServer as createHttpServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const bin = process.env.ANCHOR_BIN ?? join(repo, "target/debug/anchor");
const shots = join(repo, "ui/test-results/screens");

let proc: ChildProcess | null = null;
let dir = "";
let proc2: ChildProcess | null = null;
let dir2 = "";
let proc3: ChildProcess | null = null;
let dir3 = "";
let proc4: ChildProcess | null = null;
let dir4 = "";
let fake: Server | null = null;

test.afterAll(() => {
  proc?.kill("SIGTERM");
  proc2?.kill("SIGTERM");
  proc3?.kill("SIGTERM");
  proc4?.kill("SIGTERM");
  fake?.close();
  if (dir) rmSync(dir, { recursive: true, force: true });
  if (dir2) rmSync(dir2, { recursive: true, force: true });
  if (dir3) rmSync(dir3, { recursive: true, force: true });
  if (dir4) rmSync(dir4, { recursive: true, force: true });
});

/** A port nothing listens on (so "Ollama" is surely not running there). */
async function closedPort(): Promise<number> {
  return new Promise((ok) => {
    const srv = createServer();
    srv.listen(0, "127.0.0.1", () => {
      const a = srv.address();
      const port = typeof a === "object" && a ? a.port : 1;
      srv.close(() => ok(port));
    });
  });
}

async function waitFor<T>(f: () => T | undefined, ms: number): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = f();
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error("timed out");
    await new Promise((r) => setTimeout(r, 100));
  }
}

// The window size of the user's screen.
test.use({ viewport: { width: 1385, height: 880 } });

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

  // The folder picker: type the parent, then pick the project by name. The
  // dialog's first listing (home) is held back until after the typed path's
  // listing, as on a slow machine (CI run 37705645400): the late reply must
  // not overwrite it.
  const typed = (url: string) => new URL(url).searchParams.get("path")?.startsWith(dir) ?? false;
  let release = () => {};
  const held = new Promise<void>((r) => (release = r));
  await page.route("**/api/app/dirs*", async (route) => {
    if (typed(route.request().url())) {
      await route.continue();
      release();
    } else {
      await held;
      await route.continue();
    }
  });
  const late = page.waitForResponse((r) => r.url().includes("/api/app/dirs") && !typed(r.url()));
  await page.getByRole("button", { name: "Open folder…" }).first().click();
  const picker = page.getByRole("dialog", { name: "Open a folder" });
  await picker.getByLabel("Folder path").fill(dir);
  await picker.getByRole("button", { name: "Go", exact: true }).click();
  await late;
  await page.waitForTimeout(300); // let the page apply the late reply
  await expect(picker.getByLabel("Folder path")).toHaveValue(dir);
  await picker.getByRole("button", { name: /my-project/ }).click();
  // The click loads that folder; open it only once it is shown.
  await expect(picker.getByLabel("Folder path")).toHaveValue(/my-project$/);
  await picker.getByTestId("open-here").click();
  await expect(page.getByTestId("project")).toContainText("my-project");
  await expect(page.getByRole("heading", { name: "What should Anchor do in my-project?" })).toBeVisible();
  await page.screenshot({ path: join(shots, "app-01b-new-task.png") });

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
  await expect(page.getByTestId("history-item")).toHaveCount(1);
  await expect(page.getByTestId("history-item")).toContainText("Add a README");
  await page.getByTestId("undo-2").click();
  const confirm = page.getByRole("dialog", { name: "Confirm undo" });
  await expect(confirm).toContainText("hello.py");
  await expect(confirm).toContainText("README.md");
  await confirm.getByRole("button", { name: "Undo", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "Undone" })).toBeVisible();
  expect(readFileSync(join(ws, "hello.py"), "utf8")).toBe("print('hello')\n");
  expect(existsSync(join(ws, "README.md"))).toBe(false);
  await page.screenshot({ path: join(shots, "app-04-undone.png"), fullPage: true });

  // New task, then back to the finished one from the sidebar.
  await page.getByRole("button", { name: "New task" }).click();
  await expect(page.getByLabel("Task")).toBeVisible();
  await page.getByTestId("history-item").click();
  await expect(page.getByTestId("result")).toBeVisible();

  // Every tool is a click away: the snapshot timeline opens in a drawer.
  await page.getByRole("button", { name: "Snapshots" }).click();
  await expect(page.getByRole("heading", { name: "Snapshot timeline" })).toBeVisible();
  await page.screenshot({ path: join(shots, "app-05-tools.png") });

  // Settings: the safety rules (always on) and the models file.
  await page.getByRole("button", { name: "Settings" }).click();
  await expect(page.getByText("Always ask")).toBeVisible();
  await page.screenshot({ path: join(shots, "app-06-settings.png") });
  await page.getByRole("button", { name: "Models", exact: true }).click();
  await expect(page.getByLabel("Models file")).toHaveValue(/roles\.mock\.toml$/);
  await page.getByRole("button", { name: "Close settings" }).click();
  await expect(page.getByTestId("project")).toContainText("my-project");
  expect(log).not.toMatch(/panicked/);
});

// Update #1: without --roles, models are chosen per task. On a first run
// with nothing set up, Anchor asks for no key or account: it offers local
// first, and a task gets the ways forward instead of an error.
test("first run: no model yet gives a prompt, not an error", async ({ page }) => {
  dir2 = mkdtempSync(join(tmpdir(), "anchor-app-routed-"));
  const ws = join(dir2, "fresh-project");
  cpSync(join(repo, "examples/app/workspace"), ws, { recursive: true });
  const state = join(dir2, "state");
  mkdirSync(state, { recursive: true });
  writeFileSync(
    join(state, "app.json"),
    JSON.stringify({ recent: [ws], inference: { ollama_port: await closedPort() } }),
  );
  const urlFile = join(dir2, "url");
  const env = { ...process.env, ANCHOR_STATE_DIR: state };
  delete env["ANCHOR_FREE_URL"];
  proc2 = spawn(bin, ["app", "--keys", "memory", "--ui-url-file", urlFile], { env, stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  proc2.stdout?.on("data", (d) => (log += d));
  proc2.stderr?.on("data", (d) => (log += d));
  const url = await waitFor(() => (existsSync(urlFile) ? readFileSync(urlFile, "utf8").trim() || undefined : undefined), 60_000);

  await page.goto(url);
  await page.getByRole("button", { name: /fresh-project/ }).first().click();
  await expect(page.getByRole("heading", { name: "What should Anchor do in fresh-project?" })).toBeVisible();
  const start = page.getByTestId("get-started");
  await expect(start).toContainText("Run locally — nothing leaves your machine");
  await expect(start).not.toContainText("Start free");
  await expect(page.getByTestId("path-chip")).toContainText("Models: automatic");
  await page.screenshot({ path: join(shots, "app-07-first-run.png") });

  await page.getByLabel("Task").fill("/local add a docstring to hello.py");
  await expect(page.getByTestId("path-chip")).toContainText("This task: local");
  await page.getByLabel("Task").fill("add a docstring to hello.py");
  await page.getByRole("button", { name: "Send" }).click();
  const prompt = page.getByTestId("task-prompt");
  await expect(prompt).toContainText("No model is ready to run this task yet.");
  await expect(prompt).toContainText("Add your own API key");
  await expect(page.getByLabel("Task")).toHaveValue("add a docstring to hello.py");
  await page.screenshot({ path: join(shots, "app-08-prompt.png") });

  await prompt.getByRole("button", { name: /Add your own API key/ }).click();
  await expect(page.getByRole("heading", { name: "Models", exact: true })).toBeVisible();
  await expect(page.getByText("Not offered yet")).toBeVisible();
  await expect(page.getByText("Not running")).toBeVisible();
  await expect(page.getByTestId("key-openai")).toBeVisible();
  await page.getByLabel("Project pin").selectOption("C");
  await expect(page.getByText("Saved.")).toBeVisible();
  await page.screenshot({ path: join(shots, "app-09-models.png"), fullPage: true });
  await page.getByRole("button", { name: "Close settings" }).click();
  await expect(page.getByTestId("path-chip")).toContainText("Pinned: Local");
  expect(log).not.toMatch(/panicked/);
});

// ADR-0039: builder + checker. A scripted reviewer (another family) does
// not accept the first change; its note goes back to the builder, which
// fixes it, and the reviewer accepts. The composer offers the three modes.
test("builder + checker: the reviewer sends the change back, then accepts", async ({ page }) => {
  dir3 = mkdtempSync(join(tmpdir(), "anchor-app-pair-"));
  const ws = join(dir3, "pair-project");
  cpSync(join(repo, "examples/app/workspace"), ws, { recursive: true });
  const state = join(dir3, "state");
  mkdirSync(state, { recursive: true });
  writeFileSync(join(state, "app.json"), JSON.stringify({ recent: [ws] }));
  const urlFile = join(dir3, "url");
  const app = join(repo, "examples/app");
  proc3 = spawn(
    bin,
    [
      "app",
      "--roles",
      join(app, "roles.mock.toml"),
      "--mock-script",
      join(app, "script.json"),
      "--review-mock-script",
      join(app, "review-refute.json"),
      "--review-mock-script",
      join(app, "review-approve.json"),
      "--repair-mock-script",
      join(app, "repair.json"),
      "--ui-url-file",
      urlFile,
    ],
    { env: { ...process.env, ANCHOR_STATE_DIR: state }, stdio: ["ignore", "pipe", "pipe"] },
  );
  let log = "";
  proc3.stdout?.on("data", (d) => (log += d));
  proc3.stderr?.on("data", (d) => (log += d));
  const url = await waitFor(() => (existsSync(urlFile) ? readFileSync(urlFile, "utf8").trim() || undefined : undefined), 60_000);

  await page.goto(url);
  await page.getByRole("button", { name: /pair-project/ }).first().click();
  const modes = page.getByTestId("collab");
  await expect(modes.getByRole("radio", { name: "Builder + checker" })).toHaveAttribute("aria-checked", "true");
  await expect(modes.getByRole("radio", { name: "Two builders" })).toBeVisible();
  await expect(modes.getByRole("radio", { name: "One AI" })).toBeVisible();
  await page.getByLabel("Task").fill("Add a README and make the greeting mention anchor");
  await page.getByRole("button", { name: "Send" }).click();

  // Round 1: two changes; round 2: the fix.
  for (const target of ["hello.py", "README.md", "README.md"]) {
    const card = page.getByTestId("approval-tool");
    await expect(card).toContainText(target, { timeout: 60_000 });
    // The next round's card can replace this one at once: wait for this one.
    const id = await card.getAttribute("data-id");
    await card.getByRole("button", { name: "Approve" }).click();
    await expect(page.locator(`[data-testid="approval-tool"][data-id="${id}"]`)).toHaveCount(0, { timeout: 30_000 });
  }
  const reviews = page.getByTestId("review");
  await expect(reviews).toHaveCount(2, { timeout: 60_000 });
  await expect(reviews.first()).toContainText("Reviewer · demo-reviewer");
  await expect(reviews.first()).toContainText("not that it needs Python 3");
  await expect(reviews.first()).toContainText("not accepted");
  await expect(page.getByTestId("round")).toContainText("Round 2: the builder answers the review");
  await expect(reviews.last()).toContainText("I checked the change and accept it.");
  await expect(page.getByRole("button", { name: "Send" })).toBeVisible();
  expect(readFileSync(join(ws, "README.md"), "utf8")).toContain("needs Python 3");
  await page.screenshot({ path: join(shots, "app-10-pair.png"), fullPage: true });

  // The mode is a setting: "Two builders" is remembered.
  await modes.getByRole("radio", { name: "Two builders" }).click();
  await expect(modes.getByRole("radio", { name: "Two builders" })).toHaveAttribute("aria-checked", "true");
  expect(JSON.parse(readFileSync(join(state, "app.json"), "utf8")).inference.collab).toBe("team");
  expect(log).not.toMatch(/panicked/);
});

// 0.12.0: a reply continues the task (same thread, one history entry);
// a newer release shows a banner; without Ollama, Settings offers it.
test("reply to a task, see an update, get Ollama", async ({ page }) => {
  dir4 = mkdtempSync(join(tmpdir(), "anchor-app-reply-"));
  const ws = join(dir4, "reply-project");
  cpSync(join(repo, "examples/app/workspace"), ws, { recursive: true });
  const state = join(dir4, "state");
  mkdirSync(state, { recursive: true });
  writeFileSync(
    join(state, "app.json"),
    JSON.stringify({ recent: [ws], inference: { collab: "single", ollama_port: await closedPort() } }),
  );
  const script = join(dir4, "answer.json");
  writeFileSync(script, JSON.stringify({ steps: [{ content: "It prints hello.", prompt_tokens: 100, completion_tokens: 5 }] }));
  // GitHub's releases API, faked.
  fake = createHttpServer((req, res) => {
    res.setHeader("Content-Type", "application/json");
    res.end(
      JSON.stringify({
        tag_name: "v9.0.0",
        draft: false,
        prerelease: false,
        html_url: "https://github.com/Evos09/Anchor/releases/tag/v9.0.0",
        body: "Better.",
      }),
    );
  });
  const fakePort = await new Promise<number>((ok) =>
    fake!.listen(0, "127.0.0.1", () => {
      const a = fake!.address();
      ok(typeof a === "object" && a ? a.port : 1);
    }),
  );
  const urlFile = join(dir4, "url");
  const app = join(repo, "examples/app");
  proc4 = spawn(
    bin,
    ["app", "--roles", join(app, "roles.mock.toml"), "--mock-script", script, "--ui-url-file", urlFile],
    {
      env: {
        ...process.env,
        ANCHOR_STATE_DIR: state,
        ANCHOR_APP_VERSION: "0.12.0",
        ANCHOR_UPDATE_URL: `http://127.0.0.1:${fakePort}/repos/x`,
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let log = "";
  proc4.stdout?.on("data", (d) => (log += d));
  proc4.stderr?.on("data", (d) => (log += d));
  const url = await waitFor(() => (existsSync(urlFile) ? readFileSync(urlFile, "utf8").trim() || undefined : undefined), 60_000);

  await page.goto(url);
  await expect(page.getByTestId("update")).toContainText("Anchor 9.0.0 is available");
  await page.getByTestId("update").getByRole("button", { name: "Not now" }).click();
  await expect(page.getByTestId("update")).toHaveCount(0);

  await page.getByRole("button", { name: /reply-project/ }).first().click();
  await page.getByLabel("Task").fill("What does hello.py print?");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByTestId("result")).toHaveText("It prints hello.", { timeout: 60_000 });
  const box = page.getByLabel("Task");
  await expect(box).toHaveAttribute("placeholder", /Reply to this task/);
  await box.fill("And in French?");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByTestId("task")).toHaveCount(2);
  await expect(page.getByTestId("task").last()).toHaveText("And in French?");
  await expect(page.getByTestId("result")).toHaveCount(2, { timeout: 60_000 });
  await expect(page.getByTestId("history-item")).toHaveCount(1);
  await page.screenshot({ path: join(shots, "app-11-reply.png"), fullPage: true });

  // Settings → Models: no Ollama, so it is offered (and the test command).
  await page.getByRole("button", { name: "Settings" }).click();
  await page.getByRole("button", { name: "Models", exact: true }).click();
  await expect(page.getByRole("button", { name: "Get Ollama" })).toBeVisible();
  await page.getByRole("button", { name: "Projects", exact: true }).click();
  await expect(page.getByLabel("Test command")).toBeVisible();
  await expect(page.getByText(/No tests found/)).toBeVisible(); // loaded
  await page.getByLabel("Test command").fill("python3 -m unittest");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved.")).toBeVisible();
  await page.screenshot({ path: join(shots, "app-12-tests-setting.png"), fullPage: true });
  expect(log).not.toMatch(/panicked/);
});
