import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "./tests",
  timeout: 30000,
  expect: { timeout: 10000 },
  fullyParallel: false,
  workers: process.env.CI ? 2 : 4,
  retries: 0,
  reporter: [["list"], ["html", { open: "never" }]],
  use: {
    baseURL: "http://127.0.0.1:4173",
    headless: true,
    viewport: { width: 1440, height: 1000 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    launchOptions: {
      ...(process.env.TEST_BROWSER_EXECUTABLE
        ? { executablePath: process.env.TEST_BROWSER_EXECUTABLE }
        : {}),
      args: ["--no-sandbox"],
    },
  },
  webServer: {
    command: "npx vite preview --host 127.0.0.1 --port 4173 --strictPort",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: false,
  },
  projects: [
    { name: "ui", testMatch: "ui.test.cjs" },
    { name: "smoke", testMatch: "web-smoke.test.cjs" },
  ],
});
