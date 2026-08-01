const els = {
  form: document.querySelector("#job-form"),
  file: document.querySelector("#file-input"),
  fileLabel: document.querySelector("#file-label"),
  fileMeta: document.querySelector("#file-meta"),
  dropZone: document.querySelector("#drop-zone"),
  sourceDirectory: document.querySelector("#source-directory"),
  sourceDirectoryValue: document.querySelector("#source-directory-value"),
  mountedSaveOptions: document.querySelector("#mounted-save-options"),
  saveDirectoryGroup: document.querySelector("#save-directory-group"),
  saveDirectory: document.querySelector("#save-directory"),
  saveDirectoryLabel: document.querySelector("#save-directory-label"),
  saveDirectoryValue: document.querySelector("#save-directory-value"),
  clearSaveDirectory: document.querySelector("#clear-save-directory"),
  submit: document.querySelector("#submit-job"),
  submitLabel: document.querySelector("#submit-job-label"),
  jobsBody: document.querySelector("#jobs-body"),
  jobsCount: document.querySelector("#jobs-count"),
  empty: document.querySelector("#empty-state"),
  refresh: document.querySelector("#refresh-jobs"),
  jobsPrevious: document.querySelector("#jobs-previous"),
  jobsNext: document.querySelector("#jobs-next"),
  jobsPageLabel: document.querySelector("#jobs-page-label"),
  jobStatusFilter: document.querySelector("#job-status-filter"),
  runtimeDot: document.querySelector("#runtime-dot"),
  runtimeLabel: document.querySelector("#runtime-label"),
  runtimeModel: document.querySelector("#runtime-model"),
  runtimeMeta: document.querySelector("#runtime-meta"),
  startRuntime: document.querySelector("#start-runtime"),
  stopRuntime: document.querySelector("#stop-runtime"),
  manageModels: document.querySelector("#manage-models"),
  runtimeStartup: document.querySelector("#runtime-startup"),
  startupStage: document.querySelector("#startup-stage"),
  startupPercent: document.querySelector("#startup-percent"),
  startupProgress: document.querySelector("#startup-progress"),
  startupProgressFill: document.querySelector("#startup-progress-fill"),
  startupElapsed: document.querySelector("#startup-elapsed"),
  startupRemaining: document.querySelector("#startup-remaining"),
  startupLogDetails: document.querySelector("#startup-log-details"),
  startupLogCount: document.querySelector("#startup-log-count"),
  startupLogOutput: document.querySelector("#startup-log-output"),
  modelDialog: document.querySelector("#model-dialog"),
  closeModelDialog: document.querySelector("#close-model-dialog"),
  doneModelDialog: document.querySelector("#done-model-dialog"),
  modelList: document.querySelector("#model-list"),
  modelStorageSummary: document.querySelector("#model-storage-summary"),
  modelSource: document.querySelector("#model-source"),
  benchmarkSources: document.querySelector("#benchmark-sources"),
  benchmarkResults: document.querySelector("#benchmark-results"),
  runtimeCacheSize: document.querySelector("#runtime-cache-size"),
  clearRuntimeCache: document.querySelector("#clear-runtime-cache"),
  folderDialog: document.querySelector("#folder-dialog"),
  folderDialogTitle: document.querySelector("#folder-dialog-title"),
  folderStorage: document.querySelector("#folder-storage"),
  folderStorageLabel: document.querySelector("#folder-storage-label"),
  closeFolderDialog: document.querySelector("#close-folder-dialog"),
  cancelFolderDialog: document.querySelector("#cancel-folder-dialog"),
  selectCurrentFolder: document.querySelector("#select-current-folder"),
  folderUp: document.querySelector("#folder-up"),
  folderCurrentPath: document.querySelector("#folder-current-path"),
  folderSearchInput: document.querySelector("#folder-search-input"),
  folderKindFilter: document.querySelector("#folder-kind-filter"),
  folderList: document.querySelector("#folder-list"),
  folderEmpty: document.querySelector("#folder-empty"),
  folderSelectionInfo: document.querySelector("#folder-selection-info"),
  folderSelectionCount: document.querySelector("#folder-selection-count"),
  clearFolderSelection: document.querySelector("#clear-folder-selection"),
  showNewFolder: document.querySelector("#show-new-folder"),
  newFolderForm: document.querySelector("#new-folder-form"),
  newFolderName: document.querySelector("#new-folder-name"),
  cancelNewFolder: document.querySelector("#cancel-new-folder"),
  toast: document.querySelector("#toast"),
};

let runtimePreset = "7b-fp8";
let toastTimer;
let runtimePollTimer;
let runtimeState = "stopped";
let modelCatalog = { models: [], available_bytes: 0, benchmark: { state: "idle", results: [] } };
let modelsByPreset = new Map();
let modelStorage = null;
let lastLogText = "";
let sourceMode = "upload";
let sourceStorage = "documents";
const sourceSelections = { documents: new Map(), remote_fs: new Map() };
const sourceBrowsePaths = { documents: "", remote_fs: "" };
let saveDirectory = null;
let saveStorage = null;
let folderPurpose = "source";
let folderStorage = "documents";
let folderPath = "";
let folderParent = null;
let folderEntries = [];
let folderFilter = "all";
let folderSearch = "";
let folderError = null;
let folderDraftSelections = null;
let currentJobs = [];
let jobPageCursors = [null];
let jobPageIndex = 0;
let nextJobCursor = null;
let totalJobs = 0;
let jobPhase = "all";
let knownActiveJobs = new Map();
let jobsPollTimer;

const storageNames = {
  documents: "用户文稿",
  remote_fs: "网盘挂载",
};

const statusNames = {
  queued: "排队中",
  starting_model: "启动模型",
  translating: "翻译中",
  writing: "写入结果",
  completed: "已完成",
  failed: "失败",
  cancelled: "已取消",
};

const runtimeNames = {
  stopped: "已卸载",
  starting: "启动中",
  ready: "可用",
  stopping: "卸载中",
  failed: "启动失败",
};

const startupStageNames = {
  starting_process: "正在创建推理进程",
  loading_weights: "正在加载模型权重",
  compiling_kernels: "正在编译推理内核",
  capturing_graphs: "正在构建 CUDA Graph",
  starting_server: "正在启动推理服务",
  ready: "模型已就绪",
  failed: "模型启动失败",
};

function showToast(message) {
  clearTimeout(toastTimer);
  els.toast.textContent = message;
  els.toast.classList.add("visible");
  toastTimer = setTimeout(() => els.toast.classList.remove("visible"), 3200);
}

async function api(path, options = {}) {
  const response = await fetch(path, options);
  const type = response.headers.get("content-type") || "";
  const body = type.includes("application/json") ? await response.json() : null;
  if (!response.ok) {
    throw new Error(body?.error || `请求失败 (${response.status})`);
  }
  return body;
}

function escapeHtml(value) {
  const node = document.createElement("span");
  node.textContent = value ?? "";
  return node.innerHTML;
}

function escapeAttribute(value) {
  return String(value ?? "").replace(/[&<>"']/g, (character) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[character]);
}

function formatTime(epoch) {
  if (!epoch) return "";
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(epoch * 1000));
}

function formatDuration(seconds) {
  const value = Math.max(0, Math.round(Number(seconds) || 0));
  if (value < 60) return `${value} 秒`;
  const minutes = Math.floor(value / 60);
  const remainder = value % 60;
  return remainder ? `${minutes} 分 ${remainder} 秒` : `${minutes} 分钟`;
}

function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  if (value < 1024 * 1024 * 1024) return `${(value / 1024 / 1024).toFixed(1)} MiB`;
  return `${(value / 1024 / 1024 / 1024).toFixed(2)} GiB`;
}

function formatSpeed(bytesPerSecond) {
  return `${formatBytes(bytesPerSecond)}/s`;
}

function displayPath(path) {
  return path ? `/${path}` : "根目录";
}

function displayStoragePath(storage, path) {
  return `${storageNames[storage] || "存储"} · ${displayPath(path)}`;
}

function phaseForJob(job) {
  if (job.status === "queued") return "queued";
  if (["starting_model", "translating", "writing"].includes(job.status)) return "in_progress";
  if (job.status === "failed") return "failed";
  return "completed";
}

function jobMatchesPhase(job) {
  return jobPhase === "all" || phaseForJob(job) === jobPhase;
}

function renderJobs(jobs) {
  els.empty.hidden = jobs.length > 0;
  els.empty.querySelector("strong").textContent = jobPhase === "all"
    ? "还没有翻译任务"
    : "当前筛选下没有任务";
  els.jobsCount.textContent = `${totalJobs} 项`;
  els.jobsPageLabel.textContent = `第 ${jobPageIndex + 1} 页`;
  els.jobsPrevious.disabled = jobPageIndex === 0;
  els.jobsNext.disabled = !nextJobCursor;
  els.jobsBody.innerHTML = jobs
    .map((job) => {
      const percent = job.total > 0 ? Math.min(100, Math.round((job.completed / job.total) * 100)) : 0;
      const terminal = ["completed", "failed", "cancelled"].includes(job.status);
      const downloadable = job.status === "completed" || job.result_available;
      const partial = job.status !== "completed" && job.result_available;
      const actions = [];
      if (downloadable) {
        actions.push(`<button class="row-action" data-action="download" data-id="${job.id}" title="${partial ? "下载部分结果" : "下载结果"}" aria-label="${partial ? "下载部分结果" : "下载结果"}">↓</button>`);
      }
      if (job.status === "failed") {
        actions.push(`<button class="row-action" data-action="retry" data-id="${job.id}" title="重试任务" aria-label="重试任务">↻</button>`);
      }
      if (terminal) {
        actions.push(`<button class="row-action danger" data-action="delete" data-id="${job.id}" title="删除记录" aria-label="删除记录">×</button>`);
      } else {
        actions.push(`<button class="row-action danger" data-action="cancel" data-id="${job.id}" title="取消任务" aria-label="取消任务">×</button>`);
      }
      const route = job.source_path
        ? `${displayStoragePath(job.source_storage || "documents", job.source_path)}${job.save_path ? ` → ${displayStoragePath(job.save_storage || "documents", job.save_path)}` : ""}`
        : job.save_path
          ? `上传 → ${displayStoragePath(job.save_storage || "documents", job.save_path)}`
          : formatTime(job.created_at);
      const diagnostics = job.error
        ? `<span class="job-error" title="${escapeAttribute(job.error)}">${escapeHtml(job.error)}</span>`
        : job.failed_segments > 0
          ? `<span class="job-warning">${job.failed_segments} 段未翻译</span>`
          : "";
      const progressLabel = job.status === "completed"
        ? (job.failed_segments > 0 ? `${job.translated}/${job.total}` : "完成")
        : partial
          ? "部分结果"
          : `${job.completed}/${job.total || "-"}`;
      return `
        <tr>
          <td>
            <div class="file-cell">
              <strong>${escapeHtml(job.filename)}</strong>
              <span title="${escapeAttribute(route)}">${escapeHtml(job.target)} · ${job.mode === "replace" ? "仅译文" : "双语对照"} · ${escapeHtml(route)}</span>
              ${diagnostics}
            </div>
          </td>
          <td><span class="model-chip">${job.preset === "30b-fp8" ? "30B" : "7B"}</span></td>
          <td><span class="status-chip ${job.status}">${statusNames[job.status] || job.status}</span></td>
          <td>
            <div class="progress-wrap">
              <div class="progress-track"><div class="progress-fill" style="width:${job.status === "completed" ? 100 : percent}%"></div></div>
              <span class="progress-label">${progressLabel}</span>
            </div>
          </td>
          <td><div class="row-actions">${actions.join("")}</div></td>
        </tr>`;
    })
    .join("");
}

async function refreshJobs(silent = true) {
  try {
    const cursor = jobPageCursors[jobPageIndex];
    const query = new URLSearchParams();
    if (cursor) query.set("cursor", cursor);
    if (jobPhase !== "all") query.set("phase", jobPhase);
    const queryString = query.toString();
    const suffix = queryString ? `?${queryString}` : "";
    const page = await api(`/api/jobs${suffix}`);
    currentJobs = page.jobs || [];
    nextJobCursor = page.next_cursor || null;
    totalJobs = Number(page.total) || 0;
    if (jobPageIndex > 0 && currentJobs.length === 0) {
      jobPageCursors.pop();
      jobPageIndex -= 1;
      await refreshJobs(silent);
      return;
    }
    renderJobs(currentJobs);
  } catch (error) {
    if (!silent) showToast(error.message);
  }
}

async function refreshActiveJobs() {
  try {
    const response = await api("/api/jobs/active");
    const activeJobs = response.jobs || [];
    const activeById = new Map(activeJobs.map((job) => [job.id, job]));
    const disappeared = [...knownActiveJobs.keys()].some((id) => !activeById.has(id));
    const changedPhase = activeJobs.some((job) => {
      const previous = knownActiveJobs.get(job.id);
      return previous && phaseForJob(previous) !== phaseForJob(job);
    });
    const appearedOnFirstPage = jobPageIndex === 0
      && activeJobs.some((job) => jobMatchesPhase(job)
        && !currentJobs.some((current) => current.id === job.id));
    knownActiveJobs = activeById;
    if (disappeared || changedPhase || appearedOnFirstPage) {
      await refreshJobs();
    } else {
      currentJobs = currentJobs.map((job) => activeById.get(job.id) || job);
      renderJobs(currentJobs);
    }
  } catch (_) {
    // Runtime status already surfaces connectivity failures; keep history usable.
  } finally {
    clearTimeout(jobsPollTimer);
    jobsPollTimer = setTimeout(refreshActiveJobs, 2500);
  }
}

async function showFirstJobPage(silent = true) {
  jobPageCursors = [null];
  jobPageIndex = 0;
  await refreshJobs(silent);
}

const modelStateNames = {
  absent: "未下载",
  benchmarking: "正在测速",
  downloading: "下载中",
  paused: "已暂停",
  verifying: "正在校验",
  ready: "已安装",
  failed: "准备失败",
};

function modelName(preset) {
  return preset === "30b-fp8" ? "Hy-MT2 30B FP8" : "Hy-MT2 7B FP8";
}

function selectedModel() {
  return modelsByPreset.get(runtimePreset) || null;
}

function modelProgress(model) {
  if (!model?.expected_bytes) return 0;
  return Math.max(0, Math.min(100, Math.round((model.downloaded_bytes / model.expected_bytes) * 100)));
}

function updateRuntimeControls() {
  const model = selectedModel();
  const label = els.startRuntime.querySelector(".button-label");
  const preparing = model && ["benchmarking", "downloading", "verifying"].includes(model.state);
  if (!model || model.state === "absent") label.textContent = "下载";
  else if (model.state === "paused") label.textContent = "继续";
  else if (model.state === "failed") label.textContent = "重试";
  else if (preparing) label.textContent = `${modelProgress(model)}%`;
  else label.textContent = "启动";
  els.startRuntime.disabled = preparing || ["starting", "stopping"].includes(runtimeState);
  const submitPreset = document.querySelector('input[name="preset"]:checked')?.value || runtimePreset;
  const submitModel = modelsByPreset.get(submitPreset);
  els.submitLabel.textContent = submitModel?.state === "ready"
    ? "加入翻译队列"
    : `下载 ${submitPreset === "30b-fp8" ? "30B" : "7B"} 模型并加入队列`;
}

function renderBenchmark(benchmark) {
  const results = Array.isArray(benchmark?.results) ? benchmark.results : [];
  els.benchmarkSources.disabled = benchmark?.state === "running";
  els.benchmarkSources.textContent = benchmark?.state === "running" ? "测速中" : "重新测速";
  if (!results.length) {
    els.benchmarkResults.innerHTML = `<span class="muted">${benchmark?.state === "running" ? "正在检测三个下载来源" : "首次下载时会自动测速"}</span>`;
    return;
  }
  els.benchmarkResults.innerHTML = results.map((result) => {
    const recommended = benchmark.recommended === result.source;
    const metric = result.available
      ? `${formatSpeed(result.bytes_per_second || 0)} · ${result.latency_ms || 0} ms`
      : "无法连接";
    return `<div class="benchmark-row ${result.available ? "available" : "unavailable"}">
      <span class="source-dot" aria-hidden="true"></span>
      <strong>${escapeHtml(result.label)}</strong>
      <span>${metric}</span>
      ${recommended ? '<span class="recommended-source">推荐</span>' : ""}
    </div>`;
  }).join("");
}

function renderModelList() {
  const models = modelCatalog.models || [];
  if (modelStorage) {
    els.modelStorageSummary.textContent = `模型 ${formatBytes(modelStorage.model_bytes)} · 未完成 ${formatBytes(modelStorage.partial_bytes)} · 可用 ${formatBytes(modelStorage.available_bytes)}`;
  } else {
    els.modelStorageSummary.textContent = `可用 ${formatBytes(modelCatalog.available_bytes)}`;
  }
  els.modelList.innerHTML = models.map((model) => {
    const progress = modelProgress(model);
    const active = ["benchmarking", "downloading", "verifying"].includes(model.state);
    const meta = model.state === "ready"
      ? formatBytes(model.downloaded_bytes)
      : model.state === "absent"
        ? `需要 ${formatBytes(model.expected_bytes)}`
        : `${formatBytes(model.downloaded_bytes)} / ${formatBytes(model.expected_bytes)}${model.bytes_per_second ? ` · ${formatSpeed(model.bytes_per_second)}` : ""}`;
    let action = `<button class="button secondary" data-model-action="download" data-preset="${model.preset}" type="button">下载</button>`;
    if (active) action = `<button class="button secondary" data-model-action="pause" data-preset="${model.preset}" type="button">暂停</button>`;
    else if (model.state === "paused") action = `<button class="button secondary" data-model-action="download" data-preset="${model.preset}" type="button">继续</button>`;
    else if (model.state === "failed") action = `<button class="button secondary" data-model-action="download" data-preset="${model.preset}" type="button">重试</button>`;
    else if (model.state === "ready") action = `<button class="button danger-outline" data-model-action="delete" data-preset="${model.preset}" type="button">删除</button>`;
    return `<div class="model-row">
      <div class="model-row-main">
        <div class="model-row-title"><strong>${modelName(model.preset)}</strong><span class="model-state ${model.state}">${modelStateNames[model.state] || model.state}</span></div>
        <span>${escapeHtml(model.last_error || meta)}</span>
        ${active || model.state === "paused" ? `<div class="model-progress" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${progress}"><div style="width:${progress}%"></div></div>` : ""}
      </div>
      ${action}
    </div>`;
  }).join("");
  renderBenchmark(modelCatalog.benchmark);
}

function renderModelCatalog(catalog) {
  modelCatalog = catalog || modelCatalog;
  modelsByPreset = new Map((modelCatalog.models || []).map((model) => [model.preset, model]));
  renderModelList();
  updateRuntimeControls();
}

function renderRuntime(runtime) {
  const state = runtime.state || "failed";
  runtimeState = state;
  els.runtimeDot.className = "status-dot";
  if (state === "ready") els.runtimeDot.classList.add("ready");
  else if (["starting", "stopping"].includes(state)) els.runtimeDot.classList.add("busy");
  else if (state === "failed") els.runtimeDot.classList.add("failed");
  else els.runtimeDot.classList.add("neutral");
  els.runtimeLabel.textContent = runtimeNames[state] || state;
  const model = selectedModel();
  els.runtimeModel.textContent = runtime.preset ? modelName(runtime.preset) : modelName(runtimePreset);
  if (runtime.last_error) {
    els.runtimeMeta.textContent = runtime.last_error;
  } else if (state !== "stopped") {
    els.runtimeMeta.textContent = `${runtime.active_requests || 0} 个活动请求 · ${runtime.leases || 0} 个占用任务`;
  } else if (!model || model.state === "absent") {
    els.runtimeMeta.textContent = `尚未下载 · 需要 ${formatBytes(model?.expected_bytes || 0)}`;
  } else if (["benchmarking", "downloading", "verifying", "paused"].includes(model.state)) {
    els.runtimeMeta.textContent = `${modelStateNames[model.state]} · ${formatBytes(model.downloaded_bytes)} / ${formatBytes(model.expected_bytes)}`;
  } else if (model.state === "failed") {
    els.runtimeMeta.textContent = model.last_error || "模型准备失败";
  } else {
    els.runtimeMeta.textContent = "模型已安装 · 等待启动";
  }
  updateRuntimeControls();
  els.stopRuntime.disabled = ["stopped", "stopping"].includes(state);

  const showStartup = ["starting", "failed"].includes(state)
    && (state === "starting" || runtime.startup_stage || runtime.last_error);
  els.runtimeStartup.hidden = !showStartup;
  if (!showStartup) return;

  const progress = Math.max(0, Math.min(100, Number(runtime.startup_progress) || 0));
  const stage = runtime.startup_stage || (state === "failed" ? "failed" : "starting_process");
  els.startupStage.textContent = startupStageNames[stage] || stage;
  els.startupPercent.textContent = `${progress}%`;
  els.startupProgress.setAttribute("aria-valuenow", String(progress));
  els.startupProgressFill.style.width = `${progress}%`;
  els.startupElapsed.textContent = `已用 ${formatDuration(runtime.startup_elapsed_seconds)}`;
  if (state === "failed") {
    els.startupRemaining.textContent = "启动已中止";
  } else if (runtime.estimated_remaining_seconds == null) {
    els.startupRemaining.textContent = "正在估算剩余时间";
  } else if (runtime.estimated_remaining_seconds <= 5) {
    els.startupRemaining.textContent = "即将完成";
  } else {
    els.startupRemaining.textContent = `预计还需约 ${formatDuration(runtime.estimated_remaining_seconds)}`;
  }

  const logs = Array.isArray(runtime.recent_logs) ? runtime.recent_logs : [];
  const logText = logs.length ? logs.join("\n") : (runtime.last_error || "等待 vLLM 输出...");
  els.startupLogCount.textContent = `${logs.length} 行`;
  if (logText !== lastLogText) {
    els.startupLogOutput.textContent = logText;
    els.startupLogOutput.scrollTop = els.startupLogOutput.scrollHeight;
    lastLogText = logText;
  }
}

async function refreshRuntime(silent = true) {
  try {
    const [runtime, catalog] = await Promise.all([api("/api/runtime"), api("/api/models")]);
    renderModelCatalog(catalog);
    renderRuntime(runtime);
  } catch (error) {
    renderRuntime({ state: "failed", last_error: "无法连接算力舱 Agent" });
    if (!silent) showToast(error.message);
  }
}

async function pollRuntime() {
  await refreshRuntime();
  clearTimeout(runtimePollTimer);
  const modelBusy = (modelCatalog.models || []).some((model) => ["benchmarking", "downloading", "verifying"].includes(model.state))
    || modelCatalog.benchmark?.state === "running";
  runtimePollTimer = setTimeout(pollRuntime, runtimeState === "starting" || modelBusy ? 1000 : 2500);
}

function updateFile(file) {
  const replace = document.querySelector('input[name="mode"][value="replace"]');
  if (!file) {
    els.fileLabel.textContent = "选择文档";
    els.fileMeta.textContent = "EPUB、DOCX、字幕、TXT、Markdown";
    replace.disabled = false;
    return;
  }
  els.fileLabel.textContent = file.name;
  els.fileMeta.textContent = `${(file.size / 1024 / 1024).toFixed(2)} MB`;
  const isDocx = file.name.toLowerCase().endsWith(".docx");
  replace.disabled = isDocx;
  if (isDocx && replace.checked) {
    const bilingual = document.querySelector('input[name="mode"][value="bilingual"]');
    bilingual.checked = true;
    bilingual.dispatchEvent(new Event("change"));
  }
}

function updatePathControls() {
  const documentCount = sourceSelections.documents.size;
  const remoteCount = sourceSelections.remote_fs.size;
  const sourceCount = documentCount + remoteCount;
  const hasSource = sourceMode !== "upload" && sourceCount > 0;
  const hasSave = saveDirectory != null;
  els.sourceDirectoryValue.textContent = hasSource
    ? documentCount && remoteCount
      ? `文稿 ${documentCount} · 网盘 ${remoteCount}`
      : `${documentCount ? "用户文稿" : "网盘挂载"} · 已选 ${sourceCount} 项`
    : "选择文件或目录";
  els.sourceDirectory.title = hasSource
    ? Object.entries(sourceSelections).flatMap(([storage, selections]) =>
        [...selections.values()].map((entry) => displayStoragePath(storage, entry.path)))
      .join("\n")
    : "";
  els.saveDirectoryLabel.textContent = hasSave ? storageNames[saveStorage] : (sourceMode !== "upload" ? "保存目录" : "任务内保存");
  els.saveDirectoryValue.textContent = hasSave
    ? displayPath(saveDirectory)
    : (sourceMode !== "upload" ? "请选择保存位置" : "完成后下载");
  els.clearSaveDirectory.hidden = !hasSave;
  const mounted = sourceMode !== "upload";
  const saveStrategy = document.querySelector('input[name="save_strategy"]:checked')?.value || "sibling_suffix";
  els.mountedSaveOptions.hidden = !mounted;
  els.saveDirectoryGroup.hidden = mounted && saveStrategy !== "directory";
}

function setSourceMode(mode) {
  sourceMode = mode;
  const directory = mode !== "upload";
  els.dropZone.hidden = directory;
  els.sourceDirectory.hidden = !directory;
  els.file.required = !directory;
  els.submitLabel.textContent = directory ? "添加所选任务" : "加入翻译队列";
  updatePathControls();
}

function hideNewFolderForm() {
  els.newFolderForm.hidden = true;
  els.newFolderName.value = "";
}

function activeFolderSelections() {
  return folderDraftSelections?.[folderStorage] || sourceSelections[folderStorage];
}

function folderEntryMatches(entry) {
  if (folderFilter !== "all" && entry.kind !== folderFilter) return false;
  if (!folderSearch) return true;
  return `${entry.name} ${entry.path}`.toLocaleLowerCase().includes(folderSearch);
}

function renderSourceFolderRow(entry) {
  const selections = activeFolderSelections();
  const selected = selections.has(entry.path);
  const selectable = entry.kind === "directory" || entry.supported;
  const path = escapeAttribute(entry.path);
  const name = escapeHtml(entry.name);
  const selectLabel = selected ? `取消选择 ${entry.name}` : `选择 ${entry.name}`;
  const meta = entry.current
    ? "选择整个目录"
    : entry.kind === "directory"
      ? "打开目录"
      : entry.supported ? formatBytes(entry.size) : "不支持";
  const mainAction = entry.kind === "directory" && !entry.current
    ? `data-folder-open="${path}"`
    : `data-entry-select="${path}"`;
  return `<div class="folder-row selectable ${selected ? "selected" : ""} ${selectable ? "" : "disabled"}" role="option" aria-selected="${selected}">
    <button class="folder-checkbox" type="button" data-entry-select="${path}" aria-label="${escapeAttribute(selectLabel)}" aria-pressed="${selected}" ${selectable ? "" : "disabled"}>${selected ? "✓" : ""}</button>
    <span class="folder-row-icon" aria-hidden="true">${entry.kind === "directory" ? "▰" : "▤"}</span>
    <button class="folder-row-main" type="button" ${mainAction} ${selectable ? "" : "disabled"}>
      <strong>${name}</strong><span>${escapeHtml(meta)}</span>
    </button>
  </div>`;
}

function renderFolderEntries() {
  if (folderError) {
    els.folderList.innerHTML = `<div class="folder-error">${escapeHtml(folderError)}</div>`;
    els.folderEmpty.hidden = true;
    return;
  }

  const visible = folderEntries.filter((entry) => {
    if (folderPurpose === "save" && entry.kind !== "directory") return false;
    return folderEntryMatches(entry);
  });
  const rows = [];
  if (folderPurpose === "source") {
    const current = {
      name: folderPath ? `当前目录 · ${folderPath.split("/").at(-1)}` : "当前目录 · 根目录",
      path: folderPath,
      kind: "directory",
      supported: true,
      current: true,
    };
    if (folderEntryMatches(current)) rows.push(renderSourceFolderRow(current));
    rows.push(...visible.map(renderSourceFolderRow));
  } else {
    rows.push(...visible.map((entry) => `<button class="folder-row" type="button" data-folder-open="${escapeAttribute(entry.path)}">
      <span class="folder-row-icon" aria-hidden="true">▰</span>
      <strong>${escapeHtml(entry.name)}</strong>
      <span aria-hidden="true">›</span>
    </button>`));
  }
  els.folderList.innerHTML = rows.join("");
  els.folderEmpty.textContent = folderEntries.length ? "没有匹配项" : "此目录为空";
  els.folderEmpty.hidden = rows.length > 0;
}

function updateFolderSelectionUI() {
  const sourcePicker = folderPurpose === "source";
  const count = sourcePicker
    ? folderDraftSelections.documents.size + folderDraftSelections.remote_fs.size
    : 0;
  els.folderSelectionInfo.hidden = !sourcePicker;
  els.folderSelectionCount.textContent = `已选 ${count} 项`;
  els.clearFolderSelection.disabled = count === 0;
  els.selectCurrentFolder.disabled = sourcePicker && count === 0;
  els.selectCurrentFolder.textContent = sourcePicker ? "确认选择" : "保存到此目录";
}

function setFolderFilter(filter) {
  folderFilter = filter;
  els.folderKindFilter.querySelectorAll("[data-folder-filter]").forEach((button) => {
    const selected = button.dataset.folderFilter === filter;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
  renderFolderEntries();
}

async function loadFolder(path) {
  els.folderList.innerHTML = '<div class="folder-loading">正在读取...</div>';
  els.folderEmpty.hidden = true;
  folderError = null;
  folderSearch = "";
  els.folderSearchInput.value = "";
  try {
    const query = new URLSearchParams({ storage: folderStorage, path });
    const listing = await api(`/api/documents?${query}`);
    folderPath = listing.path || "";
    folderParent = listing.parent;
    if (folderPurpose === "source") sourceBrowsePaths[folderStorage] = folderPath;
    els.folderCurrentPath.textContent = displayStoragePath(folderStorage, folderPath);
    els.folderCurrentPath.title = displayStoragePath(folderStorage, folderPath);
    els.folderUp.disabled = folderParent == null;
    folderEntries = listing.entries;
    renderFolderEntries();
    updateFolderSelectionUI();
  } catch (error) {
    folderEntries = [];
    folderError = error.message;
    renderFolderEntries();
  }
}

async function openFolderDialog(purpose) {
  folderPurpose = purpose;
  els.folderDialogTitle.textContent = purpose === "source" ? "选择文件或目录" : "选择保存目录";
  folderStorage = purpose === "source"
    ? sourceStorage
    : (saveStorage || (sourceMode === "upload" ? "documents" : sourceStorage));
  folderDraftSelections = purpose === "source" ? {
    documents: new Map(sourceSelections.documents),
    remote_fs: new Map(sourceSelections.remote_fs),
  } : null;
  folderFilter = "all";
  folderSearch = "";
  els.folderSearchInput.value = "";
  els.folderKindFilter.hidden = purpose !== "source";
  els.showNewFolder.hidden = purpose === "source";
  updateFolderStorageControl();
  setFolderFilter("all");
  updateFolderSelectionUI();
  hideNewFolderForm();
  document.body.classList.add("folder-dialog-open");
  if (!els.folderDialog.open) els.folderDialog.showModal();
  const initialPath = purpose === "source"
    ? sourceBrowsePaths[folderStorage]
    : (saveStorage === folderStorage ? saveDirectory : null);
  await loadFolder(initialPath ?? "");
}

function updateFolderStorageControl() {
  els.folderStorageLabel.textContent = storageNames[folderStorage];
  els.folderStorage.querySelectorAll("[data-folder-storage]").forEach((button) => {
    const selected = button.dataset.folderStorage === folderStorage;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  });
}

async function selectFolderStorage(storage) {
  if (storage === folderStorage) return;
  folderStorage = storage;
  updateFolderStorageControl();
  hideNewFolderForm();
  const rememberedPath = folderPurpose === "source"
    ? sourceBrowsePaths[storage]
    : (saveStorage === storage ? saveDirectory : null);
  await loadFolder(rememberedPath ?? "");
}

function closeFolderDialog() {
  hideNewFolderForm();
  els.folderDialog.close();
}

els.file.addEventListener("change", () => updateFile(els.file.files[0]));

["dragenter", "dragover"].forEach((event) => {
  els.dropZone.addEventListener(event, (e) => {
    e.preventDefault();
    els.dropZone.classList.add("dragging");
  });
});

["dragleave", "drop"].forEach((event) => {
  els.dropZone.addEventListener(event, (e) => {
    e.preventDefault();
    els.dropZone.classList.remove("dragging");
  });
});

els.dropZone.addEventListener("drop", (event) => {
  const file = event.dataTransfer.files[0];
  if (!file) return;
  const transfer = new DataTransfer();
  transfer.items.add(file);
  els.file.files = transfer.files;
  updateFile(file);
});

els.sourceDirectory.addEventListener("click", () => openFolderDialog("source"));
els.saveDirectory.addEventListener("click", () => openFolderDialog("save"));
els.clearSaveDirectory.addEventListener("click", () => {
  saveDirectory = null;
  saveStorage = null;
  updatePathControls();
});
els.folderStorage.addEventListener("click", (event) => {
  const button = event.target.closest("[data-folder-storage]");
  if (button) selectFolderStorage(button.dataset.folderStorage);
});
els.folderSearchInput.addEventListener("input", () => {
  folderSearch = els.folderSearchInput.value.trim().toLocaleLowerCase();
  renderFolderEntries();
});
els.folderKindFilter.addEventListener("click", (event) => {
  const button = event.target.closest("[data-folder-filter]");
  if (button) setFolderFilter(button.dataset.folderFilter);
});
els.clearFolderSelection.addEventListener("click", () => {
  folderDraftSelections.documents.clear();
  folderDraftSelections.remote_fs.clear();
  renderFolderEntries();
  updateFolderSelectionUI();
});
els.closeFolderDialog.addEventListener("click", closeFolderDialog);
els.cancelFolderDialog.addEventListener("click", closeFolderDialog);
els.folderDialog.addEventListener("click", (event) => {
  if (event.target === els.folderDialog) closeFolderDialog();
});
els.folderDialog.addEventListener("close", () => {
  document.body.classList.remove("folder-dialog-open");
  folderDraftSelections = null;
});
els.folderList.addEventListener("click", (event) => {
  const selection = event.target.closest("[data-entry-select]");
  if (selection && !selection.disabled) {
    const path = selection.dataset.entrySelect;
    const entry = path === folderPath
      ? {
          path,
          name: path ? path.split("/").at(-1) : "根目录",
          kind: "directory",
          supported: true,
        }
      : folderEntries.find((candidate) => candidate.path === path);
    if (entry && (entry.kind === "directory" || entry.supported)) {
      const selections = activeFolderSelections();
      if (selections.has(path)) selections.delete(path);
      else selections.set(path, { path, name: entry.name, kind: entry.kind });
      renderFolderEntries();
      updateFolderSelectionUI();
    }
    return;
  }
  const folder = event.target.closest("[data-folder-open]");
  if (folder) loadFolder(folder.dataset.folderOpen);
});
els.folderUp.addEventListener("click", () => {
  if (folderParent != null) loadFolder(folderParent);
});
els.selectCurrentFolder.addEventListener("click", () => {
  if (folderPurpose === "source") {
    sourceSelections.documents = new Map(folderDraftSelections.documents);
    sourceSelections.remote_fs = new Map(folderDraftSelections.remote_fs);
    sourceStorage = folderStorage;
    const radio = document.querySelector('input[name="source"][value="mounted"]');
    radio.checked = true;
    radio.closest(".segmented").querySelectorAll(".segment").forEach((segment) => {
      segment.classList.toggle("active", segment.contains(radio));
    });
    setSourceMode("mounted");
  } else {
    saveDirectory = folderPath;
    saveStorage = folderStorage;
  }
  updatePathControls();
  closeFolderDialog();
});
els.showNewFolder.addEventListener("click", () => {
  els.newFolderForm.hidden = false;
  els.newFolderName.focus();
});
els.cancelNewFolder.addEventListener("click", hideNewFolderForm);
els.newFolderForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const name = els.newFolderName.value.trim();
  if (!name || name.includes("/") || name.includes("\\") || name === "." || name === "..") {
    showToast("请输入有效的文件夹名称");
    return;
  }
  const path = folderPath ? `${folderPath}/${name}` : name;
  const submit = els.newFolderForm.querySelector('button[type="submit"]');
  submit.disabled = true;
  try {
    await api("/api/documents/directories", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ storage: folderStorage, path }),
    });
    hideNewFolderForm();
    await loadFolder(path);
  } catch (error) {
    showToast(error.message);
  } finally {
    submit.disabled = false;
  }
});

function selectPreset(preset) {
  runtimePreset = preset;
  document.querySelectorAll("[data-runtime-preset]").forEach((button) => {
    const selected = button.dataset.runtimePreset === preset;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  });

  const jobPreset = document.querySelector(`input[name="preset"][value="${preset}"]`);
  if (jobPreset && !jobPreset.checked) {
    jobPreset.checked = true;
    jobPreset.closest(".segmented").querySelectorAll(".segment").forEach((segment) => {
      segment.classList.toggle("active", segment.contains(jobPreset));
    });
  }
  updateRuntimeControls();
  if (runtimeState === "stopped") {
    renderRuntime({ state: "stopped", active_requests: 0, leases: 0 });
  }
}

async function requestModelDownload(preset) {
  await api(`/api/models/${preset}/download`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ source: els.modelSource.value || "auto" }),
  });
  await refreshRuntime(false);
}

async function openModelDialog() {
  if (!els.modelDialog.open) els.modelDialog.showModal();
  document.body.classList.add("model-dialog-open");
  try {
    const [catalog, storage] = await Promise.all([api("/api/models"), api("/api/storage")]);
    modelStorage = storage;
    renderModelCatalog(catalog);
    els.runtimeCacheSize.textContent = `${formatBytes(storage.cache_bytes)} · 可安全清理`;
  } catch (error) {
    showToast(error.message);
  }
}

function closeModelDialog() {
  els.modelDialog.close();
  document.body.classList.remove("model-dialog-open");
}

document.querySelectorAll(".segmented input").forEach((input) => {
  input.addEventListener("change", () => {
    input.closest(".segmented").querySelectorAll(".segment").forEach((segment) => segment.classList.remove("active"));
    input.closest(".segment").classList.add("active");
    if (input.name === "preset") selectPreset(input.value);
    if (input.name === "source") setSourceMode(input.value);
    if (input.name === "save_strategy") updatePathControls();
  });
});

document.querySelectorAll("[data-runtime-preset]").forEach((button) => {
  button.addEventListener("click", () => selectPreset(button.dataset.runtimePreset));
});

els.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (sourceMode === "upload" && !els.file.files[0]) {
    showToast("请选择文档");
    return;
  }
  const selectedSourceCount = sourceSelections.documents.size + sourceSelections.remote_fs.size;
  if (sourceMode !== "upload" && selectedSourceCount === 0) {
    showToast("请从用户文稿或网盘挂载选择文件或目录");
    return;
  }
  const selectedSaveStrategy = document.querySelector('input[name="save_strategy"]:checked')?.value || "sibling_suffix";
  if (sourceMode !== "upload" && selectedSaveStrategy === "directory" && saveDirectory == null) {
    showToast("请选择保存位置");
    return;
  }
  els.submit.disabled = true;
  try {
    const form = new FormData(els.form);
    const preset = form.get("preset");
    const model = modelsByPreset.get(preset);
    if (!model || !["ready", "benchmarking", "downloading", "verifying"].includes(model.state)) {
      await requestModelDownload(preset);
      showToast("模型开始准备，任务会在模型就绪后自动执行");
    }
    const settings = {
      batch_size: Number(form.get("batch_size")),
      context_segments: Number(form.get("context_segments")),
      cache_enabled: document.querySelector("#cache-enabled").checked,
    };
    if (sourceMode !== "upload") {
      const sources = Object.entries(sourceSelections).flatMap(([storage, selections]) =>
        [...selections.keys()].map((path) => ({ storage, path })));
      const result = await api("/api/jobs/selection", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          sources,
          save_strategy: selectedSaveStrategy,
          ...(selectedSaveStrategy === "directory" ? {
            save_storage: saveStorage,
            save_path: saveDirectory || ".",
          } : {}),
          preset: form.get("preset"),
          target: form.get("target"),
          mode: form.get("mode"),
          settings,
        }),
      });
      const skipped = (result.skipped_existing || 0) + (result.skipped_incompatible || 0);
      showToast(result.jobs.length
        ? `已加入 ${result.jobs.length} 个任务${skipped ? `，跳过 ${skipped} 个文件` : ""}`
        : "没有可加入的文件");
    } else {
      form.set("cache_enabled", String(settings.cache_enabled));
      if (saveDirectory != null) {
        form.append("save_storage", saveStorage);
        form.append("save_path", saveDirectory || ".");
      }
      await api("/api/jobs", { method: "POST", body: form });
      els.file.value = "";
      updateFile(null);
      showToast("任务已加入队列");
    }
    await showFirstJobPage(false);
  } catch (error) {
    showToast(error.message);
  } finally {
    els.submit.disabled = false;
  }
});

els.jobsBody.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-action]");
  if (!button) return;
  if (button.dataset.action === "download") {
    window.location.href = `/api/jobs/${button.dataset.id}/result`;
    return;
  }
  if (button.dataset.action === "delete" && !window.confirm("删除这条记录及其任务文件？")) {
    return;
  }
  button.disabled = true;
  try {
    if (button.dataset.action === "delete") {
      await api(`/api/jobs/${button.dataset.id}`, { method: "DELETE" });
      showToast("记录已删除");
    } else if (button.dataset.action === "retry") {
      await api(`/api/jobs/${button.dataset.id}/retry`, { method: "POST" });
      showToast("任务已重新加入队列");
    } else {
      await api(`/api/jobs/${button.dataset.id}/cancel`, { method: "POST" });
      showToast("任务已取消");
    }
    await refreshJobs();
  } catch (error) {
    showToast(error.message);
  } finally {
    button.disabled = false;
  }
});

els.startRuntime.addEventListener("click", async () => {
  els.startRuntime.disabled = true;
  try {
    const model = selectedModel();
    if (!model || model.state !== "ready") {
      await requestModelDownload(runtimePreset);
      showToast("模型下载请求已提交");
      await openModelDialog();
    } else {
      await api("/api/runtime/start", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ preset: runtimePreset }),
      });
      showToast("模型启动请求已提交");
    }
    await refreshRuntime(false);
  } catch (error) {
    showToast(error.message);
  } finally {
    updateRuntimeControls();
  }
});

els.manageModels.addEventListener("click", openModelDialog);
els.closeModelDialog.addEventListener("click", closeModelDialog);
els.doneModelDialog.addEventListener("click", closeModelDialog);
els.modelDialog.addEventListener("cancel", () => document.body.classList.remove("model-dialog-open"));

els.modelList.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-model-action]");
  if (!button) return;
  const preset = button.dataset.preset;
  const action = button.dataset.modelAction;
  if (action === "delete" && !window.confirm(`删除 ${modelName(preset)}？以后使用时需要重新下载。`)) return;
  button.disabled = true;
  try {
    if (action === "download") {
      await requestModelDownload(preset);
      showToast("模型下载已开始");
    } else if (action === "pause") {
      await api(`/api/models/${preset}/pause`, { method: "POST" });
      showToast("正在暂停下载，已下载内容会保留");
    } else if (action === "delete") {
      await api(`/api/models/${preset}`, { method: "DELETE" });
      showToast("模型已删除");
    }
    await refreshRuntime(false);
    const storage = await api("/api/storage");
    modelStorage = storage;
    renderModelList();
    els.runtimeCacheSize.textContent = `${formatBytes(storage.cache_bytes)} · 可安全清理`;
  } catch (error) {
    showToast(error.message);
  } finally {
    button.disabled = false;
  }
});

els.benchmarkSources.addEventListener("click", async () => {
  els.benchmarkSources.disabled = true;
  try {
    await api("/api/model-sources/benchmark", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ preset: runtimePreset }),
    });
    showToast("正在从算力舱测试真实模型分片");
    await refreshRuntime(false);
  } catch (error) {
    showToast(error.message);
  }
});

els.clearRuntimeCache.addEventListener("click", async () => {
  if (!window.confirm("清理推理缓存？模型不会删除，但下次启动需要重新编译。")) return;
  els.clearRuntimeCache.disabled = true;
  try {
    await api("/api/runtime-cache", { method: "DELETE" });
    const storage = await api("/api/storage");
    modelStorage = storage;
    renderModelList();
    els.runtimeCacheSize.textContent = `${formatBytes(storage.cache_bytes)} · 可安全清理`;
    showToast("推理缓存已清理");
  } catch (error) {
    showToast(error.message);
  } finally {
    els.clearRuntimeCache.disabled = false;
  }
});

els.stopRuntime.addEventListener("click", async () => {
  els.stopRuntime.disabled = true;
  try {
    await api("/api/runtime/stop", { method: "POST" });
    showToast("模型已卸载");
    await refreshRuntime(false);
  } catch (error) {
    showToast(error.message);
  } finally {
    els.stopRuntime.disabled = false;
  }
});

els.refresh.addEventListener("click", () => refreshJobs(false));
els.jobStatusFilter.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-job-phase]");
  if (!button || button.dataset.jobPhase === jobPhase) return;
  jobPhase = button.dataset.jobPhase;
  els.jobStatusFilter.querySelectorAll("[data-job-phase]").forEach((item) => {
    const active = item === button;
    item.classList.toggle("active", active);
    item.setAttribute("aria-pressed", String(active));
  });
  await showFirstJobPage(false);
});
els.jobsPrevious.addEventListener("click", async () => {
  if (jobPageIndex === 0) return;
  jobPageCursors.pop();
  jobPageIndex -= 1;
  await refreshJobs(false);
});
els.jobsNext.addEventListener("click", async () => {
  if (!nextJobCursor) return;
  jobPageCursors.push(nextJobCursor);
  jobPageIndex += 1;
  await refreshJobs(false);
});

setSourceMode("upload");
refreshJobs(false);
pollRuntime();
refreshActiveJobs();
