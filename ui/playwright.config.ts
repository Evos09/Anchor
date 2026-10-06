import { defineConfig } from "@playwright/test";

// The end-to-end test runs a real scripted team (`anchor team run --ui`)
// and drives it through the dashboard in Chromium. Set PW_CHROMIUM to use
// a preinstalled browser instead of Playwright's download.
export default defineConfig({
  testDir: "e2e",
  timeout: 240_000,
  expect: { timeout: 30_000 },
  retries: 0,
  workers: 1,
  reporter: [["list"]],
  use: {
    headless: true,
    viewport: { width: 1400, height: 1000 },
    launchOptions: process.env.PW_CHROMIUM ? { executablePath: process.env.PW_CHROMIUM } : {},
  },
});
