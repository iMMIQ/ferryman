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
    preset: "index-translate-2b",
    target: "中文",
    mode: "bilingual",
    created_at: 1789990000,
    source_path: "研究资料/" + j.filename,
    source_storage: "documents",
  }));
const modelCatalogue = () => ({
  discovery_enabled: true,
  partial: false,
  warnings: [],
  models: ["index-translate-2b", "index-translate-35b", "general-chat"].map(
    (id) => ({
      target: {
        id,
        name: id === "general-chat" ? "通用聊天模型" : id,
        model_id: id,
        base_url: "http://models.test/v1",
        adapter: id === "general-chat" ? "generic" : "index_translate",
        context_length: 2048,
        max_output_tokens: 768,
        concurrency: 4,
      },
      source: "discovered",
      category: id === "general-chat" ? "chat" : "translation",
      available: true,
      deployment_status: "succeeded",
      runtime_status: "unknown",
      devices: ["AI Pod"],
      has_api_key: false,
    }),
  ),
});
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
    catalogue: modelCatalogue(),
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
      else if (u.pathname === "/api/models/discover") data = state.catalogue;
      else if (u.pathname === "/api/jobs/selection") {
        if (state.failSelection) {
          await route.abort("failed");
          return;
        }
        data = { jobs: [{ id: "created" }], skipped_incompatible: 1 };
      } else if (u.pathname === "/api/jobs") {
        if (state.invalidUpload) {
          await route.fulfill({
            status: 200,
            contentType: "text/html",
            body: "<html>login</html>",
          });
          return;
        }
        if (state.delayUpload) await new Promise((r) => setTimeout(r, 700));
        data = {
          ...defaultJobs()[0],
          id: `uploaded-${state.posts.length}`,
          filename: "新上传文档.txt",
          status: "queued",
        };
        state.jobs.unshift(data);
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
    else if (u.pathname === "/api/jobs") {
      const phase = u.searchParams.get("phase");
      const jobs = state.jobs.filter(
        (job) =>
          !phase ||
          (phase === "in_progress"
            ? ["starting_model", "translating", "writing"].includes(job.status)
            : job.status === phase),
      );
      data = {
        jobs: state.listLimit ? jobs.slice(0, state.listLimit) : jobs,
        total: jobs.length,
        next_cursor: null,
      };
    } else if (/^\/api\/jobs\/[^/]+$/.test(u.pathname)) {
      data =
        state.jobs.find((job) => job.id === u.pathname.split("/").at(-1)) || {};
    } else if (u.pathname === "/api/runtime")
      data = {
        state: "ready",
        preset: "index-translate-2b",
        active_requests: 12,
        leases: 2,
      };
    else if (u.pathname === "/api/models") data = state.catalogue;
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
    await expect(page.locator("#translation-preset option")).toHaveCount(
      state.catalogue.models.length + 1,
    );
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

test("focused composer collapses uploads and restores keyboard focus after removal", async ({
  page,
}) =>
  withPage(page, async (page) => {
    await expect(
      page.getByRole("button", { name: "模型与 API", exact: true }),
    ).toHaveCount(1);
    await expect(page.locator(".page-heading p")).toHaveCount(0);
    await expect(page.locator(".jobs-workspace")).not.toBeVisible();
    await page.locator("#file-input").setInputFiles([
      {
        name: "first.epub",
        mimeType: "application/epub+zip",
        buffer: Buffer.from("book"),
      },
      {
        name: "second.docx",
        mimeType: "application/octet-stream",
        buffer: Buffer.from("document"),
      },
    ]);
    await expect(page.locator("#drop-zone")).toHaveCount(0);
    await expect(page.locator("#mode-hint")).toHaveText("DOCX 仅支持双语对照");
    const remove = page.getByRole("button", {
      name: "移除 first.epub",
      exact: true,
    });
    await remove.focus();
    await page.keyboard.press("Enter");
    await expect(page.locator("#upload-list li")).toHaveCount(1);
    await expect(
      page.getByRole("button", { name: "移除 second.docx", exact: true }),
    ).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.locator("#add-files")).toBeFocused();
    await expect(page.locator("#drop-zone")).toBeVisible();
    await expect(page.locator("#mode-hint")).toHaveCount(0);
    await page.locator(".settings > summary").click();
    await expect(page.locator("#save-directory")).toBeVisible();
  }));

test("motion respects reduced motion while dialogs and rapid navigation remain usable", async ({
  page,
}) =>
  withPage(page, async (page) => {
    await page.emulateMedia({ reducedMotion: "no-preference" });
    await page.locator("#models-open").click();
    const dialog = page.locator("#model-dialog");
    await expect(dialog).toBeVisible();
    assert.equal(
      await dialog.evaluate((el) => getComputedStyle(el).animationName),
      "dialog-enter",
    );
    await page.emulateMedia({ reducedMotion: "reduce" });
    assert.equal(
      await dialog.evaluate((el) => getComputedStyle(el).animationName),
      "none",
    );
    await page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(page.locator("#models-open")).toBeFocused();
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('button[data-workspace="create"]').click();
    await expect(page.locator(".submit-panel")).toBeVisible();
    assert.equal(
      await page.locator(".submit-panel").evaluate((el) => el.inert),
      false,
    );
    await expect(page.locator("#target-language")).toBeEditable();
  }));

test("a filtered task leaving the list transfers keyboard focus before its exit animation", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('[data-job-phase="in_progress"]').click();
    await expect(page.locator("#jobs-body tr")).toHaveCount(1);
    await page.locator('tr[data-id="one"] [data-action="cancel"]').focus();
    state.jobs[0].status = "completed";
    state.jobs[0].translated = state.jobs[0].total;
    await expect(page.locator("#jobs-body tr")).toHaveCount(0);
    await expect(page.locator("#refresh-jobs")).toBeFocused();
  }));

test("partial status, stable keyboard focus, details and retry", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('tr[data-id="two"] .status-chip').waitFor();
    await expect(page.locator('tr[data-id="two"] .status-chip')).toHaveText(
      "部分完成",
    );
    assert.equal(
      await page
        .locator('tr[data-id="two"] [role="progressbar"]')
        .evaluate(
          (e) =>
            Number(e.getAttribute("aria-valuenow")) /
            Number(e.getAttribute("aria-valuemax")),
        ),
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
    await page.locator('button[data-workspace="create"]').click();
    await page.locator("#submit-job").scrollIntoViewIfNeeded();
    const submitRect = await page.locator("#submit-job").boundingBox();
    assert(
      submitRect.y + submitRect.height <= 769,
      "desktop submit stays reachable",
    );
    await page.setViewportSize({ width: 1440, height: 1000 });
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('button[data-workspace="jobs"]').click();
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
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('tr[data-id="one"]').waitFor();
    state.offline = true;
    await page.locator("#refresh-jobs").click();
    await expect(page.locator("#jobs-error")).toBeVisible();
    await expect(page.locator("#runtime-startup")).not.toBeVisible();
    await expect(page.locator("#jobs-error")).toBeVisible();
    await expect(page.locator('tr[data-id="one"]')).toBeVisible();
    await shot(page, "desktop-offline");
    await page.reload();
    await page.locator('button[data-workspace="jobs"]').click();
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
    await expect(page.locator(".submission-success")).toContainText(
      "已加入 2 个任务",
    );
    await expect(page.locator("tr.new-job")).toHaveCount(2);
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
    await page.locator('input[name="mode"][value="replace"]').check();
    await page
      .locator("label.segment")
      .filter({ hasText: "覆盖原文件" })
      .click();
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

test("model management discovers chat models without local deployment controls", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator("#models-open").click();
    await expect(
      page.getByRole("dialog", { name: "模型与 API", exact: true }),
    ).toBeVisible();
    await expect(page.locator("#start-runtime")).toHaveCount(0);
    await expect(page.locator(".api-model-list")).toContainText("通用聊天模型");
    await page.locator("#discover-models").click();
    await expect
      .poll(
        () =>
          state.posts.filter((p) => p.path === "/api/models/discover").length,
      )
      .toBeGreaterThan(0);
    await shot(page, "models");
    await page.setViewportSize({ width: 390, height: 844 });
    await shot(page, "mobile-models");
    await page.keyboard.press("Escape");
    state.modelsOffline = true;
    await page.locator("#models-open").click();
    await page.route("**/api/models/discover", (route) =>
      route.fulfill({ status: 502, json: { error: "尚未获得微服用户授权" } }),
    );
    await page.locator("#discover-models").click();
    await expect(page.locator("#model-dialog")).toContainText(
      "尚未获得微服用户授权",
    );
    await expect(page.locator("#add-custom-model")).toBeEnabled();
  }));

test("standalone model manager only offers custom APIs and never starts discovery", async ({
  page,
}) =>
  withPage(
    page,
    async (page, state) => {
      await page.locator("#models-open").click();
      await expect(page.locator("#discover-models")).toHaveCount(0);
      await expect(page.locator("#model-dialog")).not.toContainText(
        /微服|自动发现/,
      );
      await expect(page.locator("#model-dialog")).toContainText("暂无模型");
      await page.route("**/api/models/probe", (route) =>
        route.fulfill({ json: { models: ["my-chat"], context_length: 4096 } }),
      );
      let saved;
      await page.route("**/api/models", async (route) => {
        if (route.request().method() === "POST") {
          saved = route.request().postDataJSON();
          await route.fulfill({ json: {} });
        } else await route.fulfill({ json: state.catalogue });
      });
      await page.locator("#add-custom-model").click();
      await page.locator('[name="api_name"]').fill("自定义模型");
      await page
        .locator('[name="api_base_url"]')
        .fill("https://example.test/v1");
      await page.locator("#read-api-models").click();
      await expect(page.locator('[name="api_model_id"]')).toHaveValue(
        "my-chat",
      );
      await page.getByRole("button", { name: "保存 API", exact: true }).click();
      await expect(page.locator("#custom-api-form")).toHaveCount(0);
      assert.equal(saved.model_id, "my-chat");
      assert.equal(
        state.posts.filter((p) => p.path === "/api/models/discover").length,
        0,
      );
    },
    undefined,
    {
      catalogue: {
        discovery_enabled: false,
        models: [],
        partial: false,
        warnings: [],
      },
    },
  ));

test("empty state hides pagination and offers creation", async ({ page }) =>
  withPage(
    page,
    async (page) => {
      await page.locator('button[data-workspace="jobs"]').click();
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
    await page.locator('button[data-workspace="jobs"]').click();
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
    await page.locator("#models-open").click();
    const dialog = page.getByRole("dialog", {
      name: "模型与 API",
      exact: true,
    });
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
      "models-open",
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
    await page.locator('button[data-workspace="jobs"]').click();
    const row = page.locator('tr[data-id="two"]');
    await row.locator('[data-action="retry"]').click();
    await row.locator(".notice").waitFor();
    await expect(row).toContainText(/正在停止/);
    await expect(page.locator("#jobs-error")).toHaveCount(0);
    await expect(page.locator("#jobs-body tr")).toHaveCount(3);
  }));

test("custom API configuration reads aliases and keeps saved keys out of the form", async ({
  page,
}) =>
  withPage(page, async (page) => {
    let saved;
    await page.route("**/api/models/probe", (route) =>
      route.fulfill({ json: { models: ["my-chat"], context_length: 4096 } }),
    );
    await page.route("**/api/models", async (route) => {
      if (route.request().method() === "POST") {
        saved = route.request().postDataJSON();
        await route.fulfill({ json: {} });
      } else await route.fulfill({ json: modelCatalogue() });
    });
    await page.locator("#models-open").click();
    await page.locator("#add-custom-model").click();
    await page.locator('[name="api_name"]').fill("自定义聊天模型");
    await page.locator('[name="api_base_url"]').fill("https://example.test/v1");
    await page.locator('[name="api_key"]').fill("test-key");
    await page.locator("#read-api-models").click();
    await expect(page.locator('[name="api_model_id"]')).toHaveValue("my-chat");
    await expect(page.locator('[name="api_context"]')).toHaveValue("4096");
    await page.getByRole("button", { name: "保存 API", exact: true }).click();
    await expect(page.locator("#custom-api-form")).toHaveCount(0);
    assert.equal(saved.api_key, "test-key");
    assert.equal(saved.adapter, "generic");
    assert.equal(saved.max_output_tokens, 2048);
  }));

test("design layouts preserve drafts and separate recent tasks from filters", async ({
  page,
}) =>
  withPage(page, async (page) => {
    await page.locator("#file-input").setInputFiles([
      {
        name: "Designing Interfaces.epub",
        mimeType: "application/epub+zip",
        buffer: Buffer.from("book"),
      },
      {
        name: "产品研究与设计说明.txt",
        mimeType: "text/plain",
        buffer: Buffer.from("notes"),
      },
    ]);
    await page.locator("#target-language").fill("日语");
    await page
      .locator("#translation-preset")
      .selectOption("index-translate-35b");
    await expect(page.locator(".model-availability")).toHaveCount(0);
    await expect(page.locator("#drop-zone")).toHaveCount(0);
    await shot(page, "desktop-create");
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('[data-job-phase="failed"]').click();
    await expect(page.locator("#jobs-body tr")).toHaveCount(1);
    await page.locator('button[data-workspace="create"]').click();
    await expect(page.locator("#upload-list li")).toHaveCount(2);
    await expect(page.locator("#target-language")).toHaveValue("日语");
    await expect(page.locator("#translation-preset")).toHaveValue(
      "index-translate-35b",
    );
    await expect(page.locator(".jobs-workspace")).not.toBeVisible();
    await page.setViewportSize({ width: 390, height: 844 });
    await page.locator('button[data-workspace="models"]').click();
    await expect(
      page.getByRole("dialog", { name: "模型与 API", exact: true }),
    ).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.locator('button[data-workspace="models"]')).toBeFocused();
    await expect(page.locator("#upload-list li")).toHaveCount(2);
    const submit = await page.locator("#submit-job").boundingBox();
    const nav = await page.locator(".workspace-tabs").boundingBox();
    assert(submit.y + submit.height <= nav.y);
    await shot(page, "mobile-selected");
    await page.setViewportSize({ width: 390, height: 420 });
    await page.locator("#target-language").scrollIntoViewIfNeeded();
    await page.locator("#target-language").fill("英语");
    await expect(page.locator("#target-language")).toHaveValue("英语");
    await page.setViewportSize({ width: 900, height: 900 });
    assert.equal(
      await page.evaluate(() => document.documentElement.scrollWidth),
      900,
    );
    await shot(page, "tablet-create");
  }));

test("invalid upload response preserves the draft instead of reporting success", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    state.invalidUpload = true;
    await page.locator("#file-input").setInputFiles({
      name: "keep.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("keep"),
    });
    await page.locator("#submit-job").click();
    await page.locator("#confirm-submit").click();
    await expect(page.locator("#submission-error")).toContainText(
      "有效的任务记录",
    );
    await expect(page.locator("#upload-list li")).toHaveCount(1);
    await expect(page.locator(".submission-success")).toHaveCount(0);
  }));

test("history reconciles terminal jobs missed by active polling", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('button[data-workspace="jobs"]').click();
    await expect(page.locator('tr[data-id="one"] .status-chip')).toHaveText(
      "翻译中",
    );
    // Simulate a task created and completed in another tab between polls: it
    // never appears in active, so only periodic history reconciliation sees it.
    state.jobs.unshift({
      ...defaultJobs()[1],
      id: "instant",
      filename: "instant.txt",
      failed_segments: 0,
    });
    await expect(page.locator('tr[data-id="instant"]')).toBeVisible({
      timeout: 15000,
    });
  }));

test("old task details fetch the terminal record outside the visible history page", async ({
  page,
}) =>
  withPage(page, async (page, state) => {
    await page.locator('button[data-workspace="jobs"]').click();
    await page.locator('tr[data-id="one"] .file-title').click();
    await expect(page.locator("#job-detail-content")).toContainText("翻译中");
    const old = state.jobs.find((job) => job.id === "one");
    old.status = "completed";
    old.translated = old.total;
    old.completed = old.total;
    old.result_available = true;
    state.jobs.unshift({
      ...defaultJobs()[1],
      id: "newest",
      filename: "newest.txt",
    });
    state.listLimit = 1;
    await expect(page.locator("#job-detail-content")).toContainText("已完成");
    await expect(
      page.locator('#job-detail-actions [data-action="cancel"]'),
    ).toHaveCount(0);
    await expect(
      page.locator("#job-detail-actions .download-action"),
    ).toBeVisible();
  }));

test("mounted submission retries preserve their idempotency key", async ({
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
    state.failSelection = true;
    await page.locator("#submit-job").click();
    await page.locator("#confirm-overwrite").check();
    await page.locator("#confirm-submit").click();
    await expect(page.locator("#submission-error")).toContainText("原提交结果");
    state.failSelection = false;
    await page.locator("#submit-job").click();
    await page.locator("#confirm-overwrite").check();
    await page.locator("#confirm-submit").click();
    await expect(page.locator(".submission-success")).toContainText(
      "已加入 1 个任务",
    );
    const attempts = state.posts
      .filter((post) => post.path === "/api/jobs/selection")
      .map((post) => JSON.parse(post.body));
    assert.equal(attempts.length, 2);
    assert(attempts[0].request_id);
    assert.equal(attempts[0].request_id, attempts[1].request_id);
  }));

test("an unrelated upload preserves an unconfirmed mounted submission", async ({
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
    await page
      .locator("label.segment")
      .filter({ hasText: "覆盖原文件" })
      .click();
    state.failSelection = true;
    await page.locator("#submit-job").click();
    await page.locator("#confirm-overwrite").check();
    await page.locator("#confirm-submit").click();
    await expect(page.locator("#submission-error")).toContainText("原提交结果");

    await page.locator("label.segment").filter({ hasText: "上传文件" }).click();
    await page.locator("#file-input").setInputFiles({
      name: "unrelated.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("unrelated"),
    });
    await page.locator("#submit-job").click();
    await page.locator("#confirm-submit").click();
    await expect(page.locator(".submission-success")).toContainText(
      "已加入 1 个任务",
    );

    await page.locator('button[data-workspace="create"]').click();
    await page
      .locator("label.segment")
      .filter({ hasText: "文稿与网盘" })
      .click();
    state.failSelection = false;
    await page.locator("#submit-job").click();
    await page.locator("#confirm-overwrite").check();
    await page.locator("#confirm-submit").click();
    await expect(page.locator("#submit-job")).not.toBeDisabled();
    const attempts = state.posts
      .filter((post) => post.path === "/api/jobs/selection")
      .map((post) => JSON.parse(post.body));
    assert.equal(attempts.length, 2);
    assert.equal(attempts[0].request_id, attempts[1].request_id);
  }));
