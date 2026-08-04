import { defineConfig, devices } from "@playwright/test";

/** Fixed so the app can be launched pointing at it before any test runs. */
export const FAKE_SERVE_PORT = 7399;
export const FAKE_SERVE_URL = `http://127.0.0.1:${FAKE_SERVE_PORT}`;

/**
 * Drives the built application, not the dev server, so the E2E test exercises
 * what actually ships. The fake `omar serve` is started per test file rather
 * than here, so each test owns its run lifecycle.
 */
export default defineConfig({
  testDir: "./tests/e2e",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? "line" : "list",
  timeout: 60_000,
  use: {
    baseURL: "http://127.0.0.1:3100",
    trace: "retain-on-failure",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npm run start -- --port 3100",
    url: "http://127.0.0.1:3100",
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    // Mode is a launch flag, so the app must be started pointing at the fake
    // daemon. Tests bind it to this fixed port.
    env: { OMAR_SERVE_URL: FAKE_SERVE_URL },
  },
});
