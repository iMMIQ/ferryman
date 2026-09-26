import { useQuery, useQueryClient } from "@tanstack/vue-query";
import { reactive, computed, onUnmounted, useId, watch } from "vue";
import { api, json, isCancelled } from "../lib/api";
import { errorText, outputName, bytes } from "../lib/format";
import type {
  ClientConfig,
  Source,
  RequestData,
  Preview,
  Listing,
  CreatedJobs,
  Preset,
} from "../lib/types";
export type SubmissionStage =
  "editing" | "previewing" | "confirming" | "submitting";
interface Snapshot {
  readonly request: RequestData;
  readonly files: readonly File[];
  readonly sourceMode: "upload" | "mounted";
  readonly preview: Preview;
}
export function useSubmission(
  download: (preset: Preset) => Promise<void>,
  onSuccess: (ids: string[], message: string) => Promise<void>,
) {
  const s = reactive({
    stage: "editing" as SubmissionStage,
    sourceMode: "upload" as "upload" | "mounted",
    files: [] as File[],
    sources: [] as Source[],
    save: null as Source | null,
    preset: "7b-fp8" as Preset,
    target: "中文",
    mode: "bilingual" as "bilingual" | "replace",
    strategy: "sibling_suffix" as RequestData["save_strategy"],
    batch: 25,
    context: 5,
    cache: true,
    maxBytes: 512 * 1024 * 1024,
    validation: "",
    error: "",
    result: "",
    progress: 0 as number | null,
    uploadLabel: "",
    uploading: false,
    acknowledged: false,
    preview: null as Preview | null,
  });
  const locked = computed(() => s.stage !== "editing"),
    hasDocx = computed(
      () =>
        s.sourceMode === "upload" &&
        s.files.some((f) => f.name.toLowerCase().endsWith(".docx")),
    );
  const client = useQueryClient();
  const previewKey = ["submission-preview", useId()];
  const config = useQuery({
    queryKey: ["config"],
    queryFn: ({ signal }) => api<ClientConfig>("/api/config", { signal }),
    staleTime: Infinity,
  });
  watch(
    config.data,
    (data) => {
      if (data) s.maxBytes = data.max_upload_bytes;
    },
    { immediate: true },
  );
  let snapshot: Snapshot | undefined,
    upload: XMLHttpRequest | undefined,
    cancelled = false;
  const attemptKey = "ferryman-mounted-submission";
  let mountedAttempt: { payload: string; id: string } | undefined;
  try {
    const saved = JSON.parse(sessionStorage.getItem(attemptKey) || "null");
    if (typeof saved?.payload === "string" && typeof saved?.id === "string")
      mountedAttempt = saved;
  } catch {
    /* Storage can be disabled; in-memory retries remain safe. */
  }
  function persistAttempt() {
    try {
      if (mountedAttempt)
        sessionStorage.setItem(attemptKey, JSON.stringify(mountedAttempt));
      else sessionStorage.removeItem(attemptKey);
    } catch {
      /* Keep the in-memory attempt when storage is unavailable. */
    }
  }
  const extensions = new Set([
    "epub",
    "docx",
    "pdf",
    "srt",
    "vtt",
    "ass",
    "ssa",
    "lrc",
    "txt",
    "md",
    "markdown",
  ]);
  function add(files: File[]) {
    if (locked.value) return;
    const errors: string[] = [];
    for (const file of files) {
      if (!extensions.has(file.name.split(".").at(-1)?.toLowerCase() || "")) {
        errors.push(`${file.name}：不支持的格式`);
        continue;
      }
      if (file.size > s.maxBytes) {
        errors.push(`${file.name}：超过 ${bytes(s.maxBytes)}`);
        continue;
      }
      if (
        !s.files.some(
          (f) =>
            f.name === file.name &&
            f.size === file.size &&
            f.lastModified === file.lastModified,
        )
      )
        s.files.push(file);
    }
    s.validation = errors.join("；");
    if (hasDocx.value) s.mode = "bilingual";
  }
  function request(): RequestData {
    const save =
      s.sourceMode === "upload" || s.strategy === "directory" ? s.save : null;
    return {
      sources: s.sources.map((x) => ({ ...x })),
      save_strategy: s.strategy,
      ...(save
        ? { save_storage: save.storage, save_path: save.path || "." }
        : {}),
      preset: s.preset,
      target: s.target.trim(),
      mode: hasDocx.value ? "bilingual" : s.mode,
      settings: {
        batch_size: s.batch,
        context_segments: s.context,
        cache_enabled: s.cache,
      },
    };
  }
  async function preview() {
    if (locked.value) return;
    s.error = "";
    s.result = "";
    if (s.sourceMode === "upload" && !s.files.length) {
      s.error = "请先选择至少一个文档。";
      return;
    }
    if (s.sourceMode === "mounted" && !s.sources.length) {
      s.error = "请选择来源文件或目录。";
      return;
    }
    if (s.sourceMode === "mounted" && s.strategy === "directory" && !s.save) {
      s.error = "请选择保存目录。";
      return;
    }
    const data = request(),
      sourceMode = s.sourceMode,
      files = s.files.slice(),
      save = s.save ? { ...s.save } : null;
    // Capture all inputs before the first await. Neither rendering nor submission
    // reads the live draft again until this operation is complete or discarded.
    s.stage = "previewing";
    try {
      const result = await client.fetchQuery({
        queryKey: [
          ...previewKey,
          data,
          files.map((f) => [f.name, f.size, f.lastModified]),
        ],
        gcTime: 0,
        queryFn: async ({ signal }): Promise<Preview> => {
          let result: Preview;
          if (sourceMode === "mounted")
            result = await api<Preview>("/api/jobs/selection/preview", {
              ...json(data),
              signal,
            });
          else {
            const existing = new Set<string>();
            if (save) {
              const listing = await api<Listing>(
                `/api/documents?${new URLSearchParams({ storage: save.storage, path: save.path })}`,
                { signal },
              );
              listing.entries.forEach((e) => existing.add(e.name));
            }
            const planned = new Set<string>();
            const entries = files.map((file) => {
              const name = outputName(file.name, data.mode),
                skip =
                  save && (existing.has(name) || planned.has(name))
                    ? "输出文件已存在或同批文件重名，不会覆盖"
                    : null;
              planned.add(name);
              return {
                source_path: file.name,
                save_path: `${save?.path ? `${save.path}/` : ""}${name}`,
                save_storage: save?.storage,
                skip_reason: skip,
              };
            });
            result = {
              files: entries,
              eligible_count: entries.filter((f) => !f.skip_reason).length,
            };
          }
          return result;
        },
      });
      snapshot = {
        request: data,
        sourceMode,
        files: files.filter((_, index) => !result.files[index]?.skip_reason),
        preview: result,
      };
      s.preview = result;
      s.acknowledged = false;
      s.stage = "confirming";
    } catch (error) {
      if (!isCancelled(error)) {
        s.error = `无法预览：${errorText(error)}`;
        s.stage = "editing";
      }
    }
  }
  function edit() {
    if (s.stage === "submitting") return;
    void client.cancelQueries({ queryKey: previewKey });
    snapshot = undefined;
    s.preview = null;
    s.stage = "editing";
  }
  function sendFile(
    file: File,
    data: RequestData,
    index: number,
    count: number,
  ) {
    return new Promise<string | undefined>((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      upload = xhr;
      s.uploading = true;
      s.progress = 0;
      s.uploadLabel = `上传 ${index + 1}/${count}：${file.name}`;
      xhr.open("POST", "/api/jobs");
      xhr.timeout = 30 * 60 * 1000;
      xhr.upload.onprogress = (event) => {
        s.progress = event.lengthComputable
          ? Math.round((event.loaded / event.total) * 100)
          : null;
      };
      xhr.onload = () => {
        upload = undefined;
        if (xhr.status >= 200 && xhr.status < 300) {
          try {
            if (
              !xhr
                .getResponseHeader("content-type")
                ?.includes("application/json")
            )
              throw new Error();
            const id = JSON.parse(xhr.responseText).id;
            if (typeof id !== "string" || !id.trim()) throw new Error();
            resolve(id);
          } catch {
            reject(
              new Error(
                "服务未返回有效的任务记录，提交结果尚未确认；请先刷新任务列表核实。",
              ),
            );
          }
        } else {
          let message = `上传失败 (${xhr.status})`;
          try {
            message = JSON.parse(xhr.responseText).error || message;
          } catch {}
          reject(new Error(message));
        }
      };
      xhr.onerror = () => reject(new Error("上传连接中断，请重试。"));
      xhr.ontimeout = () => reject(new Error("上传超时，请重试。"));
      xhr.onabort = () =>
        reject(new Error("已取消上传；已经加入队列的任务不会取消。"));
      const form = new FormData();
      form.append("file", file);
      for (const key of [
        "preset",
        "target",
        "mode",
        "save_storage",
        "save_path",
      ] as const)
        if (data[key] != null) form.append(key, String(data[key]));
      for (const [key, value] of Object.entries(data.settings))
        form.append(key, String(value));
      xhr.send(form);
    });
  }
  function cancel() {
    cancelled = true;
    upload?.abort();
  }
  async function submit() {
    if (
      s.stage !== "confirming" ||
      !snapshot ||
      !snapshot.preview.eligible_count
    )
      return;
    if (
      snapshot.preview.files.some((f) => f.overwrite && !f.skip_reason) &&
      !s.acknowledged
    )
      return;
    const captured = snapshot;
    s.stage = "submitting";
    cancelled = false;
    let added = 0;
    const ids: string[] = [];
    try {
      await download(captured.request.preset);
      if (cancelled) throw new Error("已停止提交；已加入队列的任务不会取消。");
      if (captured.sourceMode === "upload")
        for (const [index, file] of captured.files.entries()) {
          if (cancelled)
            throw new Error("已停止后续上传；已经加入队列的任务不会取消。");
          const id = await sendFile(
            file,
            captured.request,
            index,
            captured.files.length,
          );
          if (id) ids.push(id);
          added++;
          s.files = s.files.filter((f) => f !== file);
        }
      else {
        const payload = JSON.stringify(captured.request);
        if (mountedAttempt?.payload !== payload)
          mountedAttempt = { payload, id: crypto.randomUUID() };
        persistAttempt();
        const result = await api<CreatedJobs>(
          "/api/jobs/selection",
          json({ ...captured.request, request_id: mountedAttempt.id }),
          120000,
        );
        added = result.jobs.length;
        ids.push(...result.jobs.map((job) => job.id));
        const skipped =
          (result.skipped_existing || 0) +
          (result.skipped_generated || 0) +
          (result.skipped_incompatible || 0) +
          (result.skipped_unsupported || 0);
        if (skipped)
          s.result = `已加入 ${added} 个任务；提交时重新检查并跳过 ${skipped} 个文件。`;
      }
      if (!s.result) s.result = `已加入 ${added} 个任务，模型就绪后自动执行。`;
      mountedAttempt = undefined;
      persistAttempt();
      await onSuccess(ids, s.result);
    } catch (error) {
      s.error =
        captured.sourceMode === "mounted"
          ? `${errorText(error)} 请先刷新任务列表核实；以相同设置重试将查询原提交结果，不会重复创建任务。`
          : `${added ? `已加入 ${added} 个任务。` : ""}${errorText(error)} 未确认的文件已保留，请先核实任务列表。`;
    } finally {
      upload = undefined;
      s.uploading = false;
      s.stage = "editing";
      snapshot = undefined;
      s.preview = null;
    }
  }
  const summary = computed(() =>
    s.preview && snapshot
      ? `将创建 ${snapshot.preview.eligible_count} 个任务，跳过 ${snapshot.preview.files.length - snapshot.preview.eligible_count} 个文件 · ${snapshot.request.target} · ${snapshot.request.mode === "replace" ? "仅译文" : "双语对照"}`
      : "",
  );
  onUnmounted(() => {
    void client.cancelQueries({ queryKey: previewKey });
    cancel();
  });
  return { s, locked, hasDocx, add, preview, edit, submit, cancel, summary };
}
