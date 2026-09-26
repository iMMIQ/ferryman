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
let runtimeState = "unknown";
let runtimeConnected = false;
let runtimePresetActive = null;
let catalogConnected = false;
let jobsLoaded = false;
let jobsLastUpdated = null;
let jobsError = "";
let jobListRequest = 0;
let selectedFiles = [];
let uploadRequest = null;
let uploadCancelled = false;
let submissionBusy = false;
let preparedSubmission = null;
let maxUploadBytes = 512 * 1024 * 1024;
let detailJobId = null;
const $ = (id) => document.getElementById(id);
const supportedExtensions = new Set(["epub", "docx", "pdf", "srt", "vtt", "ass", "ssa", "lrc", "txt", "md", "markdown"]);

function notice(id, message) {
  $(id).hidden = !message;
  $(id).textContent = message || "";
}

function switchWorkspace(view) {
  document.body.dataset.workspace = view;
  document.querySelectorAll("[data-workspace]").forEach((button) => {
    if (button.tagName === "BUTTON") button.setAttribute("aria-pressed", String(button.dataset.workspace === view));
  });
}

document.querySelectorAll("button[data-workspace]").forEach((button) => button.addEventListener("click", () => {
  switchWorkspace(button.dataset.workspace);
}));
document.querySelectorAll("[data-close-dialog]").forEach((button) => button.addEventListener("click", () => $(button.dataset.closeDialog).close()));
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
  if (job.status === "cancelled") return "cancelled";
  if (job.status === "completed" && job.failed_segments > 0) return "partial";
  return "completed";
}

function jobMatchesPhase(job) {
  return jobPhase === "all" || phaseForJob(job) === jobPhase;
}

function isPartial(job) {
  return Boolean((job.status === "completed" && job.failed_segments > 0)
    || (job.status !== "completed" && job.result_available));
}

function jobStatusLabel(job) {
  return job.status === "completed" && job.failed_segments > 0 ? "部分完成" : statusNames[job.status] || job.status;
}

function canRetry(job) {
  return ["failed", "cancelled"].includes(job.status) || (job.status === "completed" && job.failed_segments > 0);
}

function renderJobsState() {
  if (!jobsLoaded) els.jobsCount.textContent = "—";
  $("jobs-error").hidden = !jobsError;
  $("jobs-error-text").textContent = jobsError ? `任务更新失败：${jobsError}${jobsLoaded ? "。当前显示上次成功加载的记录。" : "。暂时无法获取记录。"}` : "";
  $("jobs-updated").textContent = jobsLastUpdated ? `最近更新 ${jobsLastUpdated.toLocaleTimeString("zh-CN")}` : (jobsError ? "尚未加载任务" : "正在加载任务…");
  els.empty.hidden = currentJobs.length > 0 || !!jobsError;
  els.empty.querySelector("strong").textContent = !jobsLoaded ? "正在加载任务…" : jobPhase === "all" ? "还没有翻译任务" : "当前筛选下没有任务";
  els.empty.querySelector("span").textContent = jobPhase === "all" ? "选择文档，提交后会自动准备模型并开始翻译" : "试试其他状态，或创建新的翻译任务";
  $("empty-create").hidden = !jobsLoaded || jobPhase !== "all";
  document.querySelector(".jobs-pagination").hidden = !jobsLoaded || (jobPageIndex === 0 && !nextJobCursor);
}

function renderJobs(jobs) {
  const previousFocus = document.activeElement;
  const focusRow = previousFocus?.closest("tr[data-id]");
  const rows = new Map([...els.jobsBody.children].map((row) => [row.dataset.id, row]));
  jobs.forEach((job, index) => {
    let row = rows.get(job.id);
    if (!row) {
      row = document.createElement("tr");
      row.dataset.id = job.id;
      row.innerHTML = `<td><div class="file-cell"><button class="file-title" data-action="details"></button><span class="file-meta"></span><span class="file-diagnostic"></span></div></td>
        <td><span class="model-chip"></span></td><td><span class="status-chip"></span></td>
        <td><div class="progress-wrap"><div class="progress-track"><div class="progress-fill"></div></div><span class="progress-label"></span></div></td>
        <td><div class="row-actions"><button class="row-action download-action" data-action="download">下载</button><button class="row-action" data-action="retry">重试</button><button class="row-action" data-action="cancel">取消</button><button class="row-action" data-action="details" aria-label="更多操作及任务详情">更多</button></div></td>`;
      row.querySelectorAll("[data-action]").forEach((button) => { button.dataset.id = job.id; });
    }
    rows.delete(job.id);
    const set = (selector, text) => { const node = row.querySelector(selector); if (node.textContent !== text) node.textContent = text; return node; };
    set(".file-title", job.filename).title = job.filename;
    const route = job.source_path ? displayStoragePath(job.source_storage || "documents", job.source_path) : `上传 · ${formatTime(job.created_at)}`;
    set(".file-meta", `${job.target} · ${job.mode === "replace" ? "仅译文" : "双语对照"} · ${route}`).title = route;
    const diagnostic = set(".file-diagnostic", job.error || (job.failed_segments > 0 ? `${job.failed_segments} 段未翻译，可补译` : ""));
    diagnostic.className = `file-diagnostic ${job.error ? "job-error" : "job-warning"}`;
    diagnostic.hidden = !diagnostic.textContent;
    set(".model-chip", job.preset === "30b-fp8" ? "30B" : "7B");
    set(".status-chip", jobStatusLabel(job)).className = `status-chip ${phaseForJob(job) === "partial" ? "partial" : job.status}`;
    const percent = job.total > 0 ? Math.min(100, Math.round((job.translated / job.total) * 100)) : 0;
    const fill = row.querySelector(".progress-fill");
    fill.style.width = `${percent}%`;
    fill.classList.toggle("partial", isPartial(job));
    set(".progress-label", job.total > 0 ? `已译 ${job.translated}/${job.total}` : job.status === "queued" ? "等待调度" : "准备中");
    row.querySelector(".progress-wrap").title = `已处理 ${job.completed}/${job.total || "未知"} 段；已译 ${job.translated} 段`;
    const download = row.querySelector('[data-action="download"]');
    download.hidden = !(job.status === "completed" || job.result_available);
    download.textContent = isPartial(job) ? "下载部分" : "下载";
    download.setAttribute("aria-label", isPartial(job) ? "下载部分结果" : "下载结果");
    const retry = row.querySelector('[data-action="retry"]');
    retry.hidden = !canRetry(job);
    retry.textContent = job.status === "completed" ? "补译" : job.status === "cancelled" ? "继续" : "重试";
    row.querySelector('[data-action="cancel"]').hidden = ["completed", "failed", "cancelled"].includes(job.status);
    if (els.jobsBody.children[index] !== row) els.jobsBody.insertBefore(row, els.jobsBody.children[index] || null);
  });
  rows.forEach((row) => row.remove());
  if (focusRow && (!previousFocus.isConnected || previousFocus.hidden)) {
    const next = els.jobsBody.querySelector(".file-title") || els.refresh;
    next.focus({ preventScroll: true });
  } else if (focusRow && previousFocus.isConnected && document.activeElement !== previousFocus) {
    previousFocus.focus({ preventScroll: true });
  }
  els.jobsCount.textContent = jobsLoaded ? `${totalJobs} 项` : "";
  els.jobsPageLabel.textContent = `第 ${jobPageIndex + 1} 页`;
  els.jobsPrevious.disabled = jobPageIndex === 0;
  els.jobsNext.disabled = !nextJobCursor;
  renderJobsState();
  if (detailJobId && $("job-detail-dialog").open) updateJobDetail();
}

async function refreshJobs() {
  const request = ++jobListRequest;
  try {
    const query = new URLSearchParams();
    if (jobPageCursors[jobPageIndex]) query.set("cursor", jobPageCursors[jobPageIndex]);
    if (jobPhase !== "all") query.set("phase", jobPhase);
    const page = await api(`/api/jobs?${query}`);
    if (request !== jobListRequest) return;
    currentJobs = page.jobs || [];
    nextJobCursor = page.next_cursor || null;
    totalJobs = Number(page.total) || 0;
    jobsLoaded = true; jobsError = ""; jobsLastUpdated = new Date();
    if (jobPageIndex > 0 && currentJobs.length === 0) {
      jobPageCursors.pop(); jobPageIndex -= 1;
      return refreshJobs();
    }
    renderJobs(currentJobs);
  } catch (error) {
    if (request !== jobListRequest) return;
    jobsError = error.message;
    renderJobsState();
  }
}

async function refreshActiveJobs() {
  try {
    const response = await api("/api/jobs/active");
    const activeJobs = response.jobs || [];
    const activeById = new Map(activeJobs.map((job) => [job.id, job]));
    const disappeared = [...knownActiveJobs.keys()].some((id) => !activeById.has(id));
    const changedPhase = activeJobs.some((job) => knownActiveJobs.has(job.id) && phaseForJob(knownActiveJobs.get(job.id)) !== phaseForJob(job));
    const appeared = jobPageIndex === 0 && activeJobs.some((job) => jobMatchesPhase(job) && !currentJobs.some((current) => current.id === job.id));
    knownActiveJobs = activeById;
    $("active-job-count").textContent = activeJobs.length;
    if (jobsError || !jobsLoaded || disappeared || changedPhase || appeared) await refreshJobs();
    else {
      currentJobs = currentJobs.map((job) => activeById.get(job.id) || job);
      jobsLastUpdated = new Date();
      renderJobs(currentJobs);
    }
  } catch (error) {
    jobsError = error.message;
    renderJobsState();
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
  const preparing = model && ["benchmarking", "downloading", "verifying"].includes(model.state);
  const runningSelected = runtimeState === "ready" && runtimePresetActive === runtimePreset;
  els.startRuntime.querySelector(".button-label").textContent = !runtimeConnected || !catalogConnected ? "等待连接"
    : runningSelected ? "运行中" : !model || model.state === "absent" ? "下载模型"
    : model.state === "paused" ? "继续下载" : model.state === "failed" ? "重试" : preparing ? `${modelProgress(model)}%` : "启动模型";
  els.startRuntime.disabled = !runtimeConnected || !catalogConnected || runningSelected || preparing || ["starting", "stopping"].includes(runtimeState);
  els.stopRuntime.disabled = !runtimeConnected || ["stopped", "stopping", "unknown"].includes(runtimeState);
  els.submitLabel.textContent = submissionBusy ? "正在处理…" : "预览并提交";
  const preset = document.querySelector('input[name="preset"]:checked')?.value || runtimePreset;
  const chosen = modelsByPreset.get(preset);
  $("model-choice-summary").textContent = preset === "7b-fp8" ? "7B · 默认" : "30B · 更大模型";
  $("model-choice-info").textContent = `${chosen ? `${modelStateNames[chosen.state] || chosen.state}${chosen.state === "absent" ? ` · 下载约 ${formatBytes(chosen.expected_bytes)}` : ""}。` : "模型状态待同步。"}提交后自动准备模型，无需手动启动。`;
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
  const modelOrder = { "7b-fp8": 0, "30b-fp8": 1 };
  const models = [...(modelCatalog.models || [])]
    .sort((left, right) => (modelOrder[left.preset] ?? 99) - (modelOrder[right.preset] ?? 99));
  if (modelStorage) {
    els.modelStorageSummary.textContent = `模型目录 ${formatBytes(modelStorage.model_bytes)} · 未完成 ${formatBytes(modelStorage.partial_bytes)} · 可用 ${formatBytes(modelStorage.available_bytes)}`;
  } else {
    els.modelStorageSummary.textContent = `可用 ${formatBytes(modelCatalog.available_bytes)}`;
  }
  const rows = new Map([...els.modelList.children].map((row) => [row.dataset.preset, row]));
  models.forEach((model, index) => {
    let row = rows.get(model.preset);
    if (!row) {
      row = document.createElement("div"); row.className = "model-row"; row.dataset.preset = model.preset;
      row.innerHTML = `<div class="model-row-main"><div class="model-row-title"><strong></strong><span class="model-state"></span></div><span class="model-meta"></span><div class="model-progress" role="progressbar" aria-label="模型下载进度" aria-valuemin="0" aria-valuemax="100"><div></div></div></div><button class="button secondary" type="button"></button>`;
    }
    rows.delete(model.preset);
    const progress = modelProgress(model);
    const active = ["benchmarking", "downloading", "verifying"].includes(model.state);
    row.querySelector("strong").textContent = modelName(model.preset);
    const state = row.querySelector(".model-state"); state.textContent = modelStateNames[model.state] || model.state; state.className = `model-state ${model.state}`;
    row.querySelector(".model-meta").textContent = model.last_error || (model.state === "ready" ? formatBytes(model.downloaded_bytes) : model.state === "absent" ? `需要 ${formatBytes(model.expected_bytes)}` : `${formatBytes(model.downloaded_bytes)} / ${formatBytes(model.expected_bytes)}${model.bytes_per_second ? ` · ${formatSpeed(model.bytes_per_second)}` : ""}`);
    const bar = row.querySelector(".model-progress"); bar.hidden = !(active || model.state === "paused"); bar.setAttribute("aria-valuenow", progress); bar.firstElementChild.style.width = `${progress}%`;
    const button = row.querySelector("button");
    button.dataset.preset = model.preset;
    button.dataset.modelAction = active ? "pause" : model.state === "ready" ? "delete" : "download";
    button.textContent = active ? "暂停" : model.state === "ready" ? "删除" : model.state === "paused" ? "继续" : model.state === "failed" ? "重试" : "下载";
    button.className = `button ${model.state === "ready" ? "danger-outline" : "secondary"}`;
    if (els.modelList.children[index] !== row) els.modelList.insertBefore(row, els.modelList.children[index] || null);
  });
  rows.forEach((row) => row.remove());
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
  runtimePresetActive = runtime.preset || null;
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
  els.stopRuntime.disabled = !runtimeConnected || ["stopped", "stopping"].includes(state);

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

async function refreshRuntime() {
  const [runtime, catalog] = await Promise.allSettled([api("/api/runtime"), api("/api/models")]);
  runtimeConnected = runtime.status === "fulfilled";
  catalogConnected = catalog.status === "fulfilled";
  if (catalogConnected) renderModelCatalog(catalog.value);
  if (runtimeConnected) renderRuntime(runtime.value);
  else {
    els.runtimeDot.className = "status-dot neutral";
    els.runtimeLabel.textContent = "连接中断";
    els.runtimeMeta.textContent = "状态暂不可用，正在重连";
    els.runtimeStartup.hidden = true;
  }
  $("connection-error").hidden = runtimeConnected && catalogConnected;
  $("connection-error-text").textContent = !runtimeConnected ? "无法连接算力舱。任务记录独立保留，恢复连接后可继续。" : "模型目录读取失败，正在重试。运行状态仍可查看。";
  updateRuntimeControls();
}

async function pollRuntime() {
  await refreshRuntime();
  clearTimeout(runtimePollTimer);
  const modelBusy = (modelCatalog.models || []).some((model) => ["benchmarking", "downloading", "verifying"].includes(model.state))
    || modelCatalog.benchmark?.state === "running";
  runtimePollTimer = setTimeout(pollRuntime, runtimeState === "starting" || modelBusy ? 1000 : 2500);
}

function outputName(name, mode) {
  const index = name.lastIndexOf(".");
  const suffix = mode === "replace" ? "translated" : "bilingual";
  return index < 0 ? `${name}.${suffix}` : `${name.slice(0, index)}.${suffix}${name.slice(index)}`;
}

function updateFile() {
  els.fileLabel.textContent = selectedFiles.length ? `已选 ${selectedFiles.length} 个文件 · 点击继续添加` : "选择或拖入文档";
  els.fileMeta.textContent = selectedFiles.length ? `共 ${formatBytes(selectedFiles.reduce((sum, file) => sum + file.size, 0))}` : "EPUB、DOCX、PDF、字幕、TXT、Markdown";
  $("upload-list").innerHTML = selectedFiles.map((file, index) => `<li><span title="${escapeAttribute(file.name)}">${escapeHtml(file.name)}<small>${formatBytes(file.size)}</small></span><button type="button" class="icon-button" data-remove-file="${index}" aria-label="移除 ${escapeAttribute(file.name)}">×</button></li>`).join("");
  const hasDocx = sourceMode === "upload" && selectedFiles.some((file) => file.name.toLowerCase().endsWith(".docx"));
  const replace = document.querySelector('input[name="mode"][value="replace"]');
  replace.disabled = hasDocx;
  $("mode-hint").hidden = !hasDocx;
  if (hasDocx && replace.checked) {
    const bilingual = document.querySelector('input[name="mode"][value="bilingual"]');
    bilingual.checked = true; bilingual.dispatchEvent(new Event("change"));
  }
  updateSaveHint();
}

function addFiles(files) {
  const rejected = [];
  for (const file of files) {
    const ext = file.name.split(".").pop().toLowerCase();
    if (!supportedExtensions.has(ext)) { rejected.push(`${file.name}：不支持的格式`); continue; }
    if (file.size > maxUploadBytes) { rejected.push(`${file.name}：超过 ${formatBytes(maxUploadBytes)}`); continue; }
    if (!selectedFiles.some((item) => item.name === file.name && item.size === file.size && item.lastModified === file.lastModified)) selectedFiles.push(file);
  }
  notice("upload-validation", rejected.join("；"));
  els.file.value = "";
  updateFile();
}

function updateSaveHint() {
  const mode = document.querySelector('input[name="mode"]:checked')?.value || "bilingual";
  const example = outputName(selectedFiles[0]?.name || "book.epub", mode);
  const strategy = document.querySelector('input[name="save_strategy"]:checked')?.value;
  $("save-hint").textContent = sourceMode !== "upload" && strategy === "sibling_overwrite" ? "覆盖原文件：提交前将列出受影响文件并要求确认。"
    : `保存副本，例如 ${example}。${saveDirectory != null ? displayStoragePath(saveStorage, saveDirectory) : sourceMode === "upload" ? "完成后下载" : "与来源文件放在同一目录"}`;
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
  updateSaveHint();
}

function setSourceMode(mode) {
  sourceMode = mode;
  const directory = mode !== "upload";
  els.dropZone.hidden = directory;
  els.sourceDirectory.hidden = !directory;
  els.file.required = false;
  $("upload-list").hidden = directory;
  $("upload-hint").hidden = directory;
  $("upload-validation").hidden = true;
  updateFile();
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

els.file.addEventListener("change", () => addFiles(els.file.files));
$("upload-list").addEventListener("click", (event) => {
  const button = event.target.closest("[data-remove-file]");
  if (!button || submissionBusy) return;
  selectedFiles.splice(Number(button.dataset.removeFile), 1); updateFile();
});

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
  if (!submissionBusy) addFiles(event.dataTransfer.files);
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

function selectPreset(preset, syncJob = true) {
  runtimePreset = preset;
  document.querySelectorAll("[data-runtime-preset]").forEach((button) => {
    const selected = button.dataset.runtimePreset === preset;
    button.classList.toggle("active", selected);
    button.setAttribute("aria-pressed", String(selected));
  });

  const jobPreset = document.querySelector(`input[name="preset"][value="${preset}"]`);
  if (syncJob && jobPreset && !jobPreset.checked) {
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
    if (input.name === "mode") updateSaveHint();
  });
});

document.querySelectorAll("[data-runtime-preset]").forEach((button) => {
  button.addEventListener("click", () => selectPreset(button.dataset.runtimePreset, false));
});

function submissionRequest() {
  const form = new FormData(els.form);
  return {
    sources: Object.entries(sourceSelections).flatMap(([storage, selections]) => [...selections.keys()].map((path) => ({storage, path}))),
    save_strategy: form.get("save_strategy"),
    ...(saveDirectory != null ? {save_storage: saveStorage, save_path: saveDirectory || "."} : {}),
    preset: form.get("preset"), target: form.get("target"), mode: form.get("mode"),
    settings: {batch_size: Number(form.get("batch_size")), context_segments: Number(form.get("context_segments")), cache_enabled: $("cache-enabled").checked},
  };
}

els.form.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (submissionBusy) return;
  notice("submission-error", "");
  const request = submissionRequest();
  if (sourceMode === "upload" && !selectedFiles.length) { notice("submission-error", "请先选择至少一个文档。"); return; }
  if (sourceMode !== "upload" && !request.sources.length) { notice("submission-error", "请选择来源文件或目录。"); return; }
  if (sourceMode !== "upload" && request.save_strategy === "directory" && saveDirectory == null) { notice("submission-error", "请选择保存目录。"); return; }
  submissionBusy = true; els.submit.disabled = true; updateRuntimeControls();
  try {
    const files = sourceMode === "upload" ? selectedFiles.slice() : [];
    let preview;
    if (sourceMode === "upload") {
      const existing = new Set();
      if (saveDirectory != null) {
        const query = new URLSearchParams({storage: saveStorage, path: saveDirectory});
        const listing = await api(`/api/documents?${query}`);
        listing.entries.forEach((entry) => existing.add(entry.name));
      }
      const pendingNames = new Set();
      const previewFiles = files.map((file) => {
        const name = outputName(file.name, request.mode);
        const skip_reason = saveDirectory != null && (existing.has(name) || pendingNames.has(name)) ? "输出文件已存在或同批文件重名，不会覆盖" : null;
        pendingNames.add(name);
        return {source_path: file.name, save_path: `${saveDirectory ? `${saveDirectory}/` : ""}${name}`, save_storage: saveStorage, skip_reason};
      });
      preview = {files: previewFiles, eligible_count: previewFiles.filter((file) => !file.skip_reason).length};
    } else preview = await api("/api/jobs/selection/preview", {method:"POST", headers:{"Content-Type":"application/json"}, body:JSON.stringify(request)});
    preparedSubmission = {request, files: files.filter((_, index) => !preview.files[index].skip_reason), sourceMode};
    const overwrite = preview.files.filter((file) => file.overwrite && !file.skip_reason).length;
    $("submission-summary").textContent = `将创建 ${preview.eligible_count} 个任务${preview.files.length > preview.eligible_count ? `，跳过 ${preview.files.length - preview.eligible_count} 个文件` : ""} · ${request.target} · ${request.mode === "replace" ? "仅译文" : "双语对照"}${overwrite ? ` · 覆盖 ${overwrite} 个原文件` : ""}`;
    $("submission-files").innerHTML = preview.files.map((file) => `<div class="preview-file ${file.skip_reason ? "skipped" : ""}"><strong>${escapeHtml(file.source_storage ? displayStoragePath(file.source_storage, file.source_path) : file.source_path)}</strong><span>${file.skip_reason ? `跳过：${escapeHtml(file.skip_reason)}` : `${file.overwrite ? "覆盖原文件" : "保存副本"} → ${escapeHtml(file.save_storage ? displayStoragePath(file.save_storage, file.save_path) : `完成后下载 / ${file.save_path}`)}`}</span></div>`).join("");
    $("overwrite-confirmation").hidden = !overwrite;
    $("confirm-overwrite").checked = false;
    $("confirm-submit").dataset.eligible = preview.eligible_count;
    $("confirm-submit").disabled = !preview.eligible_count || overwrite > 0;
    $("submission-dialog").showModal();
  } catch (error) { notice("submission-error", `无法预览：${error.message}`); }
  finally { submissionBusy = false; els.submit.disabled = false; updateRuntimeControls(); }
});

$("confirm-overwrite").addEventListener("change", () => { $("confirm-submit").disabled = !$("confirm-overwrite").checked || !Number($("confirm-submit").dataset.eligible); });

function uploadFile(file, request, index, count) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest(); uploadRequest = xhr;
    $("upload-progress-label").textContent = `上传 ${index + 1}/${count}：${file.name}`;
    $("upload-meter").value = 0;
    xhr.open("POST", "/api/jobs");
    xhr.timeout = 30 * 60 * 1000;
    xhr.upload.onprogress = (event) => {
      $("upload-progress-label").textContent = `上传 ${index + 1}/${count}：${file.name}${event.lengthComputable ? ` · ${Math.round(event.loaded / event.total * 100)}%` : ""}`;
      if (event.lengthComputable) $("upload-meter").value = event.loaded / event.total * 100;
      else $("upload-meter").removeAttribute("value");
    };
    xhr.onload = () => {
      uploadRequest = null;
      let response; try { response = JSON.parse(xhr.responseText); } catch { response = {}; }
      if (xhr.status >= 200 && xhr.status < 300) resolve(response);
      else reject(new Error(response.error || `上传失败 (${xhr.status})`));
    };
    xhr.onerror = () => { uploadRequest = null; reject(new Error("上传连接中断，请重试。")); };
    xhr.ontimeout = () => { uploadRequest = null; reject(new Error("上传超时，请重试。")); };
    xhr.onabort = () => { uploadRequest = null; reject(new Error("已取消上传；已经加入队列的任务不会取消。")); };
    const form = new FormData(); form.append("file", file);
    for (const key of ["preset", "target", "mode", "save_storage", "save_path"]) if (request[key] != null) form.append(key, request[key]);
    for (const [key, value] of Object.entries(request.settings)) form.append(key, String(value));
    xhr.send(form);
  });
}

$("cancel-upload").addEventListener("click", () => { uploadCancelled = true; uploadRequest?.abort(); });
$("confirm-submit").addEventListener("click", async () => {
  if (!preparedSubmission || submissionBusy) return;
  const submission = preparedSubmission;
  $("submission-dialog").close();
  submissionBusy = true; uploadCancelled = false;
  els.submit.disabled = true; updateRuntimeControls();
  els.form.querySelectorAll("input, select, button:not(#cancel-upload)").forEach((control) => { control.dataset.wasDisabled = String(control.disabled); control.disabled = true; });
  let added = 0;
  try {
    const model = modelsByPreset.get(submission.request.preset);
    if (!model || !["ready", "benchmarking", "downloading", "verifying"].includes(model.state)) await requestModelDownload(submission.request.preset);
    if (submission.sourceMode === "upload") {
      $("upload-progress").hidden = false;
      for (const [index, file] of submission.files.entries()) {
        if (uploadCancelled) throw new Error("已停止后续上传；已经加入队列的任务不会取消。");
        await uploadFile(file, submission.request, index, submission.files.length);
        added += 1; selectedFiles = selectedFiles.filter((item) => item !== file); updateFile();
      }
    } else {
      const result = await api("/api/jobs/selection", {method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(submission.request)});
      added = result.jobs.length;
      const skipped = (result.skipped_existing || 0) + (result.skipped_incompatible || 0) + (result.skipped_unsupported || 0) + (result.skipped_generated || 0);
      if (skipped) notice("submission-error", `已加入 ${added} 个任务；提交时发现 ${skipped} 个文件不兼容、已生成译文或输出已存在，已跳过。`);
    }
    showToast(`已加入 ${added} 个任务，模型就绪后自动执行`);
    switchWorkspace("jobs");
  } catch (error) { notice("submission-error", `${added ? `已加入 ${added} 个任务。` : ""}${error.message} 未提交的文件已保留。`); }
  finally {
    submissionBusy = false; $("upload-progress").hidden = true;
    els.form.querySelectorAll("[data-was-disabled]").forEach((control) => { control.disabled = control.dataset.wasDisabled === "true"; delete control.dataset.wasDisabled; });
    els.submit.disabled = false; updateFile(); updateRuntimeControls();
    await showFirstJobPage(); await refreshActiveJobs();
  }
});

function updateJobDetail() {
  const job = currentJobs.find((item) => item.id === detailJobId);
  if (!job) return;
  const content = $("job-detail-content");
  const fields = {
    "文件": job.filename, "状态": jobStatusLabel(job), "翻译": `${job.target} · ${job.mode === "replace" ? "仅译文" : "双语对照"} · ${modelName(job.preset)}`,
    "进度": `已处理 ${job.completed}/${job.total} 段，已译 ${job.translated} 段，失败 ${job.failed_segments} 段`,
    "来源": job.source_path ? displayStoragePath(job.source_storage, job.source_path) : "上传文件",
    "保存": job.save_path ? displayStoragePath(job.save_storage, job.save_path) : "任务内保存，完成后下载",
    "创建时间": formatTime(job.created_at), "错误": job.error || "无",
  };
  if (!content.firstElementChild) {
    content.innerHTML = `<dl>${Object.keys(fields).map((label) => `<dt>${label}</dt><dd></dd>`).join("")}</dl><p class="helper-text">重试会优先复用本任务已完成的翻译。旧版本任务若缺少翻译记录，可能需要重新翻译。</p>`;
  }
  Object.values(fields).forEach((value, index) => {
    const node = content.querySelectorAll("dd")[index];
    if (node.textContent !== value) node.textContent = value;
  });
  // Keep action nodes stable while progress changes.
  const actions = $("job-detail-actions");
  if (actions.dataset.id !== job.id) {
    actions.dataset.id = job.id;
    actions.innerHTML = `<button class="button primary" data-action="download">下载结果</button><button class="button secondary" data-action="retry">重试</button><button class="button secondary" data-action="cancel">取消任务</button><button class="button danger-outline" data-action="delete">删除记录</button>`;
    actions.querySelectorAll("button").forEach((button) => { button.dataset.id = job.id; });
  }
  const download = actions.querySelector('[data-action="download"]');
  download.hidden = !(job.status === "completed" || job.result_available); download.textContent = isPartial(job) ? "下载部分结果" : "下载结果";
  const retry = actions.querySelector('[data-action="retry"]'); retry.hidden = !canRetry(job); retry.textContent = job.status === "completed" ? "补译未完成段落" : "继续 / 重试";
  actions.querySelector('[data-action="cancel"]').hidden = ["completed", "failed", "cancelled"].includes(job.status);
  actions.querySelector('[data-action="delete"]').hidden = !["completed", "failed", "cancelled"].includes(job.status);
}

async function handleJobAction(event) {
  const button = event.target.closest("[data-action]");
  if (!button || button.disabled) return;
  const {action, id} = button.dataset;
  if (action === "details") {
    detailJobId = id; notice("job-detail-error", ""); updateJobDetail(); $("job-detail-dialog").showModal(); return;
  }
  if (action === "download") { window.location.href = `/api/jobs/${id}/result`; return; }
  if (action === "delete" && !window.confirm("删除这条记录及任务内文件？保存到文稿或网盘的结果不会删除。")) return;
  button.disabled = true;
  try {
    await api(`/api/jobs/${id}${action === "delete" ? "" : `/${action}`}`, {method: action === "delete" ? "DELETE" : "POST"});
    showToast(action === "delete" ? "记录已删除" : action === "retry" ? "已排队，将复用已完成的翻译" : "任务已取消，已生成的部分结果仍可下载");
    if (action === "delete") $("job-detail-dialog").close();
    await refreshJobs();
  } catch (error) {
    if ($("job-detail-dialog").open) notice("job-detail-error", error.message);
    else { jobsError = error.message; renderJobsState(); }
  } finally { button.disabled = false; }
}
els.jobsBody.addEventListener("click", handleJobAction);
$("job-detail-actions").addEventListener("click", handleJobAction);

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
    updateRuntimeControls();
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

$("retry-jobs").addEventListener("click", () => refreshJobs());
$("retry-connection").addEventListener("click", () => refreshRuntime());
$("empty-create").addEventListener("click", () => { switchWorkspace("create"); els.file.click(); });
setSourceMode("upload");
api("/api/config").then((config) => {
  maxUploadBytes = config.max_upload_bytes || maxUploadBytes;
  $("upload-hint").textContent = `支持多文件 · 单文件最大 ${formatBytes(maxUploadBytes)}`;
}).catch(() => {});
refreshJobs();
pollRuntime();
refreshActiveJobs();
