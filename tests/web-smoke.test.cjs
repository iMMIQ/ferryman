const { test, expect } = require("@playwright/test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const os = require("node:os");
const net = require("node:net");
const http = require("node:http");
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
  let delayInference = true;
  const seen = [],
    timers = new Set();
  const inference = http.createServer((req, res) => {
    res.setHeader("Content-Type", "application/json");
    assert.equal(req.headers["x-hc-user-ticket"], undefined);
    assert.equal(req.headers.cookie, undefined);
    assert.equal(req.headers.authorization, "Bearer smoke-private-key");
    if (req.url === "/v1/models")
      return res.end(JSON.stringify({ data: [{ id: "test-chat" }] }));
    if (req.url === "/health")
      return res.end(JSON.stringify({ status: "ready", max_context: 2048 }));
    if (req.url !== "/v1/chat/completions") {
      res.statusCode = 404;
      return res.end("{}");
    }
    let body = "";
    req.on("data", (data) => (body += data));
    req.on("end", () => {
      const request = JSON.parse(body);
      seen.push(request);
      const prompt = request.messages.at(-1).content;
      const tags = [...prompt.matchAll(/<c(\d+)>[\s\S]*?<\/c\1>/g)];
      const content = tags.length
        ? tags.map((m) => `<c${m[1]}>已翻译</c${m[1]}>`).join("\n")
        : "已翻译";
      const reply = () => {
        if (!res.destroyed)
          res.end(
            JSON.stringify({
              choices: [{ message: { content }, finish_reason: "stop" }],
            }),
          );
      };
      if (delayInference) {
        const timer = setTimeout(() => {
          timers.delete(timer);
          reply();
        }, 5000);
        timers.add(timer);
      } else reply();
    });
  });
  await new Promise((resolve) => inference.listen(0, "127.0.0.1", resolve));
  const inferenceBase = `http://127.0.0.1:${inference.address().port}/v1`;
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
        FERRYMAN_MODEL_DISCOVERY_URL: "http://127.0.0.1:1/must-not-be-called",
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
    const catalogue = await (await fetch(`${origin}/api/models`)).json();
    assert.equal(catalogue.discovery_enabled, false);
    assert(
      [404, 405].includes(
        (await fetch(`${origin}/api/models/discover`, { method: "POST" }))
          .status,
      ),
    );
    const html = await (await fetch(origin)).text();
    assert.match(html, /\/assets\/index-[\w-]+\.js/);
    assert.doesNotMatch(html, /\/src\/main\.ts/);
    const asset = html.match(/src="([^"]+\.js)"/)[1];
    assert.equal((await fetch(origin + asset)).status, 200);
    const errors = [];
    page.on("pageerror", (e) => errors.push(e.message));
    await page.goto(origin);
    await page.locator("#empty-create").waitFor();
    await page.locator("#models-open").click();
    await expect(page.locator("#discover-models")).toHaveCount(0);
    await page.locator("#add-custom-model").click();
    await page.locator('[name="api_name"]').fill("Smoke chat");
    await page.locator('[name="api_base_url"]').fill(inferenceBase);
    await page.locator('[name="api_key"]').fill("smoke-private-key");
    await page.locator("#read-api-models").click();
    await expect(page.locator('[name="api_model_id"]')).toHaveValue(
      "test-chat",
    );
    await page.getByRole("button", { name: "保存 API", exact: true }).click();
    await expect(page.locator("#custom-api-form")).toHaveCount(0);
    await page.keyboard.press("Escape");
    const modelResponse = await (await fetch(`${origin}/api/models`)).text();
    assert(!modelResponse.includes("smoke-private-key"));
    const modelId = JSON.parse(modelResponse).models[0].target.id;
    const otherModels = await (
      await fetch(`${origin}/api/models`, {
        headers: { safe_uid: "another-user" },
      })
    ).json();
    assert.deepEqual(otherModels.models, []);

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
    // Exercise cancellation while the external model request is in flight.
    const createdResponse = await fetch(`${origin}/api/jobs/selection`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        sources: [{ storage: "documents", path: "smoke.txt" }],
        save_strategy: "sibling_suffix",
        model_id: modelId,
        target: "中文",
        mode: "bilingual",
      }),
    });
    assert.equal(createdResponse.status, 201);
    const created = await createdResponse.json();
    assert.equal(created.jobs.length, 1);
    assert.equal(created.jobs[0].model.name, "Smoke chat");
    await page.keyboard.press("Escape");
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator("#refresh-jobs").click();
    await page
      .locator(`tr[data-id="${created.jobs[0].id}"] .file-title`)
      .click();
    await expect.poll(() => seen.length).toBeGreaterThan(0);
    await page.locator('#job-detail-actions [data-action="cancel"]').click();
    await page.waitForFunction(() =>
      document
        .querySelector("#job-detail-content")
        ?.textContent.includes("已取消"),
    );
    await page.locator('#job-detail-actions [data-action="delete"]').click();
    // Cancellation is acknowledged before partial output and worker cleanup finish.
    await expect(async () => {
      await page.getByRole("button", { name: "确认删除", exact: true }).click();
      await expect(page.locator("#empty-create")).toBeVisible({
        timeout: 1000,
      });
    }).toPass({ timeout: 10000 });
    assert.deepEqual(
      (await (await fetch(`${origin}/api/jobs`)).json()).jobs,
      [],
    );
    delayInference = false;
    const completedResponse = await fetch(`${origin}/api/jobs/selection`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        sources: [{ storage: "documents", path: "smoke.txt" }],
        save_strategy: "sibling_suffix",
        model_id: modelId,
        target: "中文",
        mode: "bilingual",
      }),
    });
    assert.equal(completedResponse.status, 201);
    const completedId = (await completedResponse.json()).jobs[0].id;
    await expect
      .poll(
        async () =>
          (await (await fetch(`${origin}/api/jobs/${completedId}`)).json())
            .status,
      )
      .toBe("completed");
    const result = await (
      await fetch(`${origin}/api/jobs/${completedId}/result`)
    ).text();
    assert(result.includes("已翻译"));
    assert.equal(seen.at(-1).model, "test-chat");
    assert.equal(seen.at(-1).messages[0].role, "system");
    assert.equal(seen.at(-1).max_tokens, 1024);
    assert.equal(seen.at(-1).enable_thinking, undefined);
    assert.equal(seen.at(-1).top_k, undefined);
    assert.deepEqual(errors, []);
  } finally {
    if (child.exitCode === null && !spawnError) {
      child.kill("SIGTERM");
      await once(child, "exit");
    }
    for (const timer of timers) clearTimeout(timer);
    inference.closeAllConnections();
    await new Promise((resolve) => inference.close(resolve));
    await fs.rm(root, { recursive: true, force: true });
  }
});
