const { test, expect } = require("@playwright/test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");
const net = require("node:net");
const { spawn } = require("node:child_process");
const { once } = require("node:events");
test("built frontend uses real Rust config, storage, preview and job lifecycle APIs", async ({
  page,
}) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "ferryman-web-smoke-"));
  const socket = net.createServer();
  await new Promise((r) => socket.listen(0, "127.0.0.1", r));
  const port = socket.address().port;
  await new Promise((r) => socket.close(r));
  const documents = path.join(root, "documents"),
    remote = path.join(root, "remote");
  await fs.mkdir(path.join(documents, "local-development-user"), {
    recursive: true,
  });
  await fs.mkdir(path.join(remote, "local-development-user"), {
    recursive: true,
  });
  await fs.writeFile(
    path.join(documents, "local-development-user", "smoke.txt"),
    "A real preview fixture.",
  );
  const origin = `http://127.0.0.1:${port}`;
  const child = spawn(
    process.env.FERRYMAN_SMOKE_BINARY ||
      path.resolve("target/debug/ferryman-web"),
    [],
    {
      env: {
        ...process.env,
        FERRYMAN_WEB_LISTEN: `127.0.0.1:${port}`,
        FERRYMAN_WEB_DIR: path.resolve("dist/web"),
        FERRYMAN_DATA_DIR: path.join(root, "data"),
        FERRYMAN_USER_DOCUMENTS_DIR: documents,
        FERRYMAN_REMOTE_FS_DIR: remote,
        FERRYMAN_ALLOW_LOCAL_USER: "true",
        FERRYMAN_AGENT_TOKEN: "smoke-test-token-only",
        FERRYMAN_AGENT_URL: "http://127.0.0.1:1",
        RUST_LOG: "warn",
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let logs = "",
    spawnError;
  child.stderr.on("data", (b) => (logs += b));
  child.stdout.on("data", (b) => (logs += b));
  child.on("error", (e) => (spawnError = e));
  try {
    await expect
      .poll(
        async () => {
          if (spawnError) throw spawnError;
          if (child.exitCode !== null) throw new Error(logs);
          try {
            return (await fetch(`${origin}/api/config`)).ok;
          } catch {
            return false;
          }
        },
        { timeout: 10000, message: "Rust web server should become ready" },
      )
      .toBe(true);
    const config = await (await fetch(`${origin}/api/config`)).json();
    assert(config.max_upload_bytes > 0);
    const html = await (await fetch(origin)).text();
    assert.match(html, /\/assets\/index-[\w-]+\.js/);
    assert.doesNotMatch(html, /\/src\/main\.ts/);
    const asset = html.match(/src="([^"]+\.js)"/)[1];
    assert.equal((await fetch(origin + asset)).status, 200);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(origin);
    await page.locator("#empty-create").waitFor();
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    await page.locator("#source-directory").click();
    await page
      .locator('.folder-checkbox[data-entry-select="smoke.txt"]')
      .click();
    await page.locator("#select-current-folder").click();
    await page.locator("#submit-job").click();
    await page.locator("#submission-dialog").waitFor();
    await expect(page.locator("#submission-files")).toContainText(
      /smoke.bilingual.txt/,
    );
    await expect(page.locator("#submission-summary")).toContainText(/1 个任务/);
    const jobs = await (await fetch(`${origin}/api/jobs`)).json();
    assert.deepEqual(jobs.jobs, []);
    assert.equal(jobs.total, 0);
    // Exercise a real job lifecycle without a GPU. Jobs waiting for an
    // unavailable agent must remain cancellable and deletable through the UI.
    const createdResponse = await fetch(`${origin}/api/jobs/selection`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        sources: [{ storage: "documents", path: "smoke.txt" }],
        save_strategy: "sibling_suffix",
        preset: "7b-fp8",
        target: "中文",
        mode: "bilingual",
      }),
    });
    assert.equal(createdResponse.status, 201);
    const created = await createdResponse.json();
    assert.equal(created.jobs.length, 1);
    await page.keyboard.press("Escape");
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator("#refresh-jobs").click();
    await page
      .locator(`tr[data-id="${created.jobs[0].id}"] .file-title`)
      .click();
    await page.locator('#job-detail-actions [data-action="cancel"]').click();
    await page.waitForFunction(() =>
      document
        .querySelector("#job-detail-content")
        ?.textContent.includes("已取消"),
    );
    await page.locator('#job-detail-actions [data-action="delete"]').click();
    await page.getByRole("button", { name: "确认删除", exact: true }).click();
    await page.locator("#empty-create").waitFor();
    assert.deepEqual(
      (await (await fetch(`${origin}/api/jobs`)).json()).jobs,
      [],
    );
    assert.deepEqual(errors, []);
  } finally {
    if (child.exitCode === null && !spawnError) {
      child.kill("SIGTERM");
      await once(child, "exit");
    }
    await fs.rm(root, { recursive: true, force: true });
  }
});
