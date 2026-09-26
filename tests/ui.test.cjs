const { test, expect } = require("@playwright/test");
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const path = require("node:path");
const screenshotDir = process.env.UI_SCREENSHOT_DIR;
test.beforeAll(async () => {
  if (screenshotDir) await fs.mkdir(screenshotDir, { recursive: true });
});
const defaultJobs = () =>
  [
    {
      id: "one",
      filename:
        "The Design of Everyday Things — Revised and Expanded Edition.epub",
      status: "translating",
      total: 420,
      completed: 148,
      translated: 148,
      failed_segments: 0,
    },
    {
      id: "two",
      filename: "季度财务分析报告与附录.docx",
      status: "completed",
      total: 100,
      completed: 100,
      translated: 96,
      failed_segments: 4,
      result_available: true,
    },
    {
      id: "three",
      filename: "字幕合集 — 第三集.srt",
      status: "failed",
      total: 120,
      completed: 8,
      translated: 8,
      failed_segments: 1,
      error:
        "连接推理服务失败：请求超时，请检查算力舱连接后重试。完整诊断信息需要在手机上能够阅读。",
    },
  ].map((j) => ({
    ...j,
    preset: "7b-fp8",
    target: "中文",
    mode: "bilingual",
    created_at: 1789990000,
    source_path: "研究资料/" + j.filename,
    source_storage: "documents",
  }));
async function withPage(
  page,
  fn,
  viewport = { width: 1440, height: 1000 },
  initial = {},
) {
  const state = {
    jobs: defaultJobs(),
    offline: false,
    modelsOffline: false,
    posts: [],
    delayUpload: false,
    maxUploadBytes: 512 * 1024 * 1024,
    ...initial,
  };
  await page.setViewportSize(viewport);
  page.setDefaultTimeout(10000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.route("**/api/**", async (route) => {
    const req = route.request(),
      u = new URL(req.url());
    if (
      state.offline ||
      (state.modelsOffline && u.pathname === "/api/models")
    ) {
      await route.fulfill({ status: 503, json: { error: "测试连接中断" } });
      return;
    }
    let data = {};
    if (req.method() === "POST") {
      state.posts.push({ path: u.pathname, body: req.postData() });
      if (u.pathname === "/api/jobs/selection/preview")
        data = {
          eligible_count: 1,
          files: [
            {
              source_path: "Books/book.txt",
              source_storage: "documents",
              save_path: "Books/book.txt",
              save_storage: "documents",
              overwrite: true,
              skip_reason: null,
            },
            {
              source_path: "Books/manual.docx",
              save_path: "Books/manual.docx",
              overwrite: true,
              skip_reason: "DOCX 仅支持双语对照",
            },
          ],
        };
      else if (u.pathname === "/api/jobs/selection")
        data = { jobs: [{ id: "created" }], skipped_incompatible: 1 };
      else if (u.pathname === "/api/jobs") {
        if (state.delayUpload) await new Promise((r) => setTimeout(r, 700));
        data = { id: "uploaded" };
      } else if (u.pathname.endsWith("/retry")) {
        if (state.delayMutation) await new Promise((r) => setTimeout(r, 800));
        const j = state.jobs.find((j) => u.pathname.includes(j.id));
        if (j) j.status = "queued";
      }
    } else if (u.pathname === "/api/config")
      data = { max_upload_bytes: state.maxUploadBytes };
    else if (u.pathname === "/api/jobs/active")
      data = {
        jobs: state.jobs.filter((j) =>
          ["queued", "translating"].includes(j.status),
        ),
      };
    else if (u.pathname === "/api/jobs")
      data = { jobs: state.jobs, total: state.jobs.length, next_cursor: null };
    else if (u.pathname === "/api/runtime")
      data = {
        state: "ready",
        preset: "7b-fp8",
        active_requests: 12,
        leases: 2,
      };
    else if (u.pathname === "/api/models")
      data = {
        models: [
          {
            preset: "7b-fp8",
            state: "ready",
            downloaded_bytes: 8e9,
            expected_bytes: 8e9,
          },
          { preset: "30b-fp8", state: "absent", expected_bytes: 32e9 },
        ],
        available_bytes: 91e9,
        benchmark: { state: "idle", results: [] },
      };
    else if (u.pathname === "/api/storage")
      data = {
        model_bytes: 8e9,
        partial_bytes: 0,
        cache_bytes: 2.4e9,
        available_bytes: 91e9,
      };
    else if (u.pathname === "/api/documents")
      data = {
        path: "",
        parent: null,
        entries: [
          { name: "Books", path: "Books", kind: "directory", supported: false },
          {
            name: "manual.docx",
            path: "manual.docx",
            kind: "file",
            supported: true,
            size: 1024,
          },
        ],
      };
    try {
      await route.fulfill({ json: data });
    } catch (e) {
      if (!state.delayUpload) throw e;
    }
  });
  {
    await page.goto("/");
    await expect(page.locator("#runtime-label")).not.toHaveText("连接中");
    await fn(page, state);
    assert.deepEqual(errors, []);
  }
}
async function shot(page, name) {
  if (screenshotDir)
    await page.screenshot({
      path: path.join(screenshotDir, name + ".png"),
      fullPage: false,
    });
}

test("partial status, stable keyboard focus, details and retry", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('tr[data-id="two"] .status-chip').waitFor();
    await expect(page.locator('tr[data-id="two"] .status-chip')).toHaveText(
      "部分完成",
    );
    assert.equal(
      await page
        .locator('tr[data-id="two"] progress')
        .evaluate((e) => e.value / e.max),
      0.96,
    );
    const cancel = page.locator('tr[data-id="one"] [data-action="cancel"]');
    await cancel.focus();
    await page.evaluate(() => {
      window.focusedButton = document.activeElement;
    });
    state.jobs[0].translated = 150;
    await page.waitForFunction(() =>
      document
        .querySelector('tr[data-id="one"] .progress-label')
        ?.textContent.includes("150"),
    );
    assert.equal(
      await page.evaluate(
        () =>
          document.activeElement === window.focusedButton &&
          focusedButton.isConnected,
      ),
      true,
    );
    await shot(page, "desktop-workspace");
    await page.setViewportSize({ width: 1366, height: 768 });
    const submitRect = await page.locator("#submit-job").boundingBox();
    assert(
      submitRect.y + submitRect.height <= 768,
      "desktop submit stays reachable",
    );
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.locator('tr[data-id="two"] .file-title').click();
    await expect(page.locator("#job-detail-content")).toContainText(
      /季度财务分析报告与附录.docx/,
    );
    await shot(page, "task-details");
    await page.locator('#job-detail-actions [data-action="retry"]').click();
    assert(state.posts.some((p) => p.path === "/api/jobs/two/retry"));
  }));

test("offline errors persist without false empty history or model failure", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('tr[data-id="one"]').waitFor();
    state.offline = true;
    await page.locator("#refresh-jobs").click();
    await page.waitForFunction(
      () => document.querySelector("#runtime-label").textContent === "连接中断",
    );
    await expect(page.locator("#runtime-label")).toHaveText("连接中断");
    await expect(page.locator("#runtime-startup")).not.toBeVisible();
    await expect(page.locator("#jobs-error")).toBeVisible();
    await expect(page.locator('tr[data-id="one"]')).toBeVisible();
    await shot(page, "desktop-offline");
    await page.reload();
    await page.locator("#jobs-error").waitFor();
    await expect(page.locator("#empty-state")).not.toBeVisible();
    await expect(page.locator(".jobs-pagination")).not.toBeVisible();
    state.offline = false;
    await page.locator("#retry-jobs").click();
    await page.waitForFunction(() => !document.querySelector("#jobs-error"));
  }));

test("mobile navigation keeps submission and history reachable", async ({
  page,
}) =>
  withPage(
    page,
    async (page) => {
      const rect = await page.locator("#submit-job").boundingBox();
      assert(rect.y >= 0 && rect.y + rect.height <= 844);
      await expect(page.locator(".jobs-section")).not.toBeVisible();
      assert.equal(
        await page.evaluate(() => document.documentElement.scrollWidth),
        390,
      );
      await shot(page, "mobile-create");
      await page.locator('button[data-workspace="jobs"]').click();
      await expect(page.locator(".jobs-section")).toBeVisible();
      await expect(page.locator("#submit-job")).not.toBeVisible();
      await shot(page, "mobile-tasks");
      await page.setViewportSize({ width: 360, height: 740 });
      assert.equal(
        await page.evaluate(() => document.documentElement.scrollWidth),
        360,
      );
    },
    { width: 390, height: 844 },
  ));

test("multi-file validation and upload preview submit each selected file", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator("#file-input").setInputFiles([
      { name: "one.txt", mimeType: "text/plain", buffer: Buffer.from("one") },
      {
        name: "two.docx",
        mimeType: "application/octet-stream",
        buffer: Buffer.from("two"),
      },
      {
        name: "bad.exe",
        mimeType: "application/octet-stream",
        buffer: Buffer.from("bad"),
      },
    ]);
    await expect(page.locator("#upload-list li")).toHaveCount(2);
    await expect(page.locator("#upload-validation")).toContainText(/不支持/);
    await expect(
      page.locator('input[name="mode"][value="replace"]'),
    ).toBeDisabled();
    await expect(page.locator("#mode-hint")).toBeVisible();
    await page.locator("#submit-job").click();
    await page.locator("#submission-dialog").waitFor();
    await expect(page.locator("#submission-summary")).toContainText(/2 个任务/);
    await shot(page, "upload-preview");
    await page.locator("#confirm-submit").click();
    await page.waitForFunction(
      () => !document.querySelector("#submit-job").disabled,
    );
    assert.equal(state.posts.filter((p) => p.path === "/api/jobs").length, 2);
    await expect(page.locator("#upload-list li")).toHaveCount(0);
  }));

test("cancel upload retains unsubmitted files and shows persistent feedback", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    state.delayUpload = true;
    await page.locator("#file-input").setInputFiles({
      name: "keep.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("keep"),
    });
    await page.locator("#submit-job").click();
    await page.locator("#confirm-submit").click();
    await page.locator("#cancel-upload").click();
    await page.waitForFunction(
      () => !document.querySelector("#submit-job").disabled,
    );
    await expect(page.locator("#upload-list li")).toHaveCount(1);
    await expect(page.locator("#submission-error")).toContainText(/取消|停止/);
  }));

test("mounted preview shows skipped files and requires overwrite acknowledgement", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    await page.locator("#source-directory").click();
    await page.locator('.folder-checkbox[data-entry-select="Books"]').click();
    await shot(page, "directory-picker");
    await page.locator("#select-current-folder").click();
    await page
      .locator('input[name="mode"][value="replace"]')
      .check({ force: true });
    await page
      .locator('input[name="save_strategy"][value="sibling_overwrite"]')
      .check({ force: true });
    await page.locator("#submit-job").click();
    await page.locator("#submission-dialog").waitFor();
    await expect(page.locator("#submission-files")).toContainText(
      /DOCX 仅支持/,
    );
    await expect(page.locator("#confirm-submit")).toBeDisabled();
    assert.equal(
      state.posts.filter((p) => p.path === "/api/jobs/selection").length,
      0,
    );
    await shot(page, "overwrite-preview");
    await page.locator("#confirm-overwrite").check();
    await page.locator("#confirm-submit").click();
    await page.waitForFunction(
      () => !document.querySelector("#submit-job").disabled,
    );
    assert.equal(
      state.posts.filter((p) => p.path === "/api/jobs/selection").length,
      1,
    );
  }));

test("runtime management is secondary and does not offer redundant start", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await expect(page.locator("#start-runtime")).not.toBeVisible();
    await page.locator("#manage-models").click();
    await expect(page.locator("#start-runtime")).toBeDisabled();
    await expect(page.locator("#start-runtime")).toContainText(/运行中/);
    await page.locator("#model-list button").first().focus();
    await page.evaluate(() => {
      window.modelAction = document.activeElement;
    });
    await page.waitForResponse((response) =>
      response.url().endsWith("/api/models"),
    );
    assert.equal(
      await page.evaluate(() => document.activeElement === window.modelAction),
      true,
    );
    await shot(page, "models");
    await page.setViewportSize({ width: 390, height: 844 });
    const done = await page.locator("#done-model-dialog").boundingBox();
    assert(done.y + done.height <= 844);
    await shot(page, "mobile-models");
    await page.keyboard.press("Escape");
    state.modelsOffline = true;
    await expect(page.locator("#connection-error")).toContainText("模型目录");
    await expect(page.locator("#runtime-label")).toHaveText("可用");
  }));

test("empty state hides pagination and offers creation", async ({ page }) =>
  withPage(
    page,
    async (page) => {
      await page.locator("#empty-create").waitFor();
      await expect(page.locator(".jobs-pagination")).not.toBeVisible();
      await shot(page, "desktop-empty");
    },
    { width: 1440, height: 1000 },
    { jobs: [] },
  ));

test("drag and drop accepts multiple files and rejects oversized inputs", async ({
  page,
}) =>
  withPage(
    page,
    async (page) => {
      await page.evaluate(() => {
        const transfer = new DataTransfer();
        transfer.items.add(new File(["one"], "one.txt"));
        transfer.items.add(new File(["two"], "two.md"));
        transfer.items.add(new File(["too large"], "large.txt"));
        document
          .querySelector("#drop-zone")
          .dispatchEvent(
            new DragEvent("drop", { dataTransfer: transfer, bubbles: true }),
          );
      });
      await expect(page.locator("#upload-list li")).toHaveCount(2);
      await expect(page.locator("#upload-validation")).toContainText(
        /large.txt.*超过/,
      );
      await page.locator('[data-remove-file="0"]').click();
      await expect(page.locator("#upload-list li")).toHaveCount(1);
      await expect(page.locator("#upload-list")).toContainText(/two.md/);
    },
    { width: 1440, height: 1000 },
    { maxUploadBytes: 4 },
  ));

test("late directory responses cannot overwrite another storage or remembered path", async ({
  page,
}) =>
  withPage(page, async (page) => {
    const aborted = [];
    await page.route("**/api/documents?**", async (route) => {
      const storage = new URL(route.request().url()).searchParams.get(
        "storage",
      );
      await new Promise((r) =>
        setTimeout(r, storage === "documents" ? 350 : 10),
      );
      try {
        await route.fulfill({
          json: {
            path: storage === "documents" ? "OLD_DOCS" : "NEW_REMOTE",
            parent: "",
            entries: [],
          },
        });
      } catch {
        aborted.push(storage);
      }
    });
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    await page.locator("#source-directory").click();
    await page.locator('[data-folder-storage="remote_fs"]').click();
    await page.waitForFunction(() =>
      document
        .querySelector("#folder-current-path")
        .textContent.includes("NEW_REMOTE"),
    );
    await page.waitForTimeout(450);
    await expect(page.locator("#folder-current-path")).toContainText(
      /网盘挂载.*NEW_REMOTE/,
    );
    await expect(page.locator("#folder-dialog")).not.toContainText(/OLD_DOCS/);
    await page.locator(".folder-checkbox").click();
    await page.locator("#select-current-folder").click();
    await page.locator("#submit-job").click();
    await page.locator("#submission-dialog").waitFor();
    await page.keyboard.press("Escape");
    await page.locator("#source-directory").click();
    await expect(page.locator("#folder-current-path")).toContainText(
      /NEW_REMOTE|正在读取/,
    );
  }));

test("preview locks the entire draft and closing it allows a fresh snapshot", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    await page.locator("#source-directory").click();
    await page.locator('.folder-checkbox[data-entry-select="Books"]').click();
    await page.locator("#select-current-folder").click();
    await page.route("**/api/jobs/selection/preview", async (route) => {
      state.posts.push({
        path: "/api/jobs/selection/preview",
        body: route.request().postData(),
      });
      await new Promise((r) => setTimeout(r, 300));
      await route.fulfill({
        json: {
          eligible_count: 1,
          files: [
            {
              source_path: "Books/book.txt",
              source_storage: "documents",
              save_path: "Books/book.bilingual.txt",
              save_storage: "documents",
            },
          ],
        },
      });
    });
    await page.locator("#target-language").fill("English");
    await page.locator("#submit-job").click();
    await expect(page.locator("#target-language")).toBeDisabled();
    await expect(
      page.locator('input[name="source"][value="upload"]'),
    ).toBeDisabled();
    await page.locator("#submission-dialog").waitFor();
    await expect(page.locator("#submission-summary")).toContainText(/English/);
    await page.keyboard.press("Escape");
    await page.locator("#target-language").fill("日本語");
    await page.locator("#submit-job").click();
    await page.locator("#confirm-submit").click();
    await page.waitForFunction(
      () => !document.querySelector("#submit-job").disabled,
    );
    assert.equal(
      JSON.parse(state.posts.find((p) => p.path === "/api/jobs/selection").body)
        .target,
      "日本語",
    );
  }));

test("cancelled preview cannot open a dialog after its delayed response", async ({
  page,
}) =>
  withPage(page, async (page) => {
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    await page.locator("#source-directory").click();
    await page.locator('.folder-checkbox[data-entry-select="Books"]').click();
    await page.locator("#select-current-folder").click();
    await page.route("**/api/jobs/selection/preview", async (route) => {
      await new Promise((r) => setTimeout(r, 300));
      try {
        await route.fulfill({ json: { eligible_count: 0, files: [] } });
      } catch {}
    });
    await page.locator("#submit-job").click();
    await page.locator("#cancel-preview").click();
    await page.waitForTimeout(400);
    await expect(page.locator("#submission-dialog")).toHaveCount(0);
    await expect(page.locator("#target-language")).not.toBeDisabled();
  }));

test("job mutations share pending state between table and details", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    state.delayMutation = true;
    const row = page.locator('tr[data-id="two"]');
    await row.locator('[data-action="retry"]').click();
    await row.locator(".file-title").click();
    const detailRetry = page.locator(
      '#job-detail-actions [data-action="retry"]',
    );
    await expect(detailRetry).toBeDisabled();
    await detailRetry.dispatchEvent("click");
    await page.waitForTimeout(100);
    assert.equal(
      state.posts.filter((p) => p.path === "/api/jobs/two/retry").length,
      1,
    );
    await page.waitForFunction(
      () =>
        !document.querySelector('#job-detail-actions [data-action="retry"]'),
    );
  }));

test("dialog traps keyboard focus and restores the opener on Escape", async ({
  page,
}) =>
  withPage(page, async (page) => {
    await page.locator("#manage-models").click();
    const dialog = page.getByRole("dialog", { name: "模型管理", exact: true });
    await dialog.waitFor();
    for (let i = 0; i < 18; i++) {
      await page.keyboard.press("Tab");
      assert.equal(
        await page.evaluate(
          () => !!document.activeElement.closest("#model-dialog"),
        ),
        true,
      );
    }
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    assert.equal(
      await page.evaluate(() => document.activeElement.id),
      "manage-models",
    );
  }));

test("job action failures stay local and preserve the list", async ({ page }) =>
  withPage(page, async (page) => {
    await page.route("**/api/jobs/two/retry", (route) =>
      route.fulfill({
        status: 409,
        json: { error: "任务正在停止，请稍后重试" },
      }),
    );
    const row = page.locator('tr[data-id="two"]');
    await row.locator('[data-action="retry"]').click();
    await row.locator(".notice").waitFor();
    await expect(row).toContainText(/正在停止/);
    await expect(page.locator("#jobs-error")).toHaveCount(0);
    await expect(page.locator("#jobs-body tr")).toHaveCount(3);
  }));

test("runtime mutation invalidates an older poll and refreshes the final state", async ({
  page,
}) =>
  withPage(page, async (page) => {
    let current = "ready",
      calls = 0,
      oldResponded = false;
    await page.route("**/api/runtime", async (route) => {
      const snapshot = current,
        call = ++calls;
      if (call === 1) await new Promise((r) => setTimeout(r, 700));
      try {
        await route.fulfill({
          json: {
            state: snapshot,
            preset: "7b-fp8",
            active_requests: 0,
            leases: 0,
          },
        });
      } catch {}
      if (call === 1) oldResponded = true;
    });
    await page.route("**/api/runtime/stop", async (route) => {
      current = "stopped";
      await route.fulfill({ json: {} });
    });
    await page.locator("#manage-models").click();
    await page.locator("#stop-runtime").click();
    await page.waitForFunction(
      () => document.querySelector("#runtime-label").textContent === "已卸载",
    );
    await expect.poll(() => oldResponded).toBe(true);
    await expect(page.locator("#runtime-label")).toHaveText("已卸载");
    await expect(page.locator("#stop-runtime")).toBeDisabled();
  }));
