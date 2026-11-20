import type { Job, Phase, Storage } from "./types";
export const storageNames: Record<Storage, string> = {
  documents: "用户文稿",
  remote_fs: "网盘挂载",
};
export const phases: { value: Phase; label: string }[] = [
  { value: "all", label: "全部" },
  { value: "queued", label: "排队中" },
  { value: "in_progress", label: "进行中" },
  { value: "completed", label: "已完成" },
  { value: "partial", label: "部分完成" },
  { value: "cancelled", label: "已取消" },
  { value: "failed", label: "失败" },
];
export function phase(job: Job): Phase {
  return job.status === "completed" && job.failed_segments > 0
    ? "partial"
    : ["starting_model", "translating", "writing"].includes(job.status)
      ? "in_progress"
      : (job.status as Phase);
}
export const retryable = (job: Job) =>
  ["partial", "failed", "cancelled"].includes(phase(job));
export const terminal = (job: Job) =>
  ["completed", "failed", "cancelled"].includes(job.status);
export function statusLabel(job: Job) {
  return (
    (
      {
        starting_model: "准备翻译",
        translating: "翻译中",
        writing: "写入结果",
      } as Record<string, string>
    )[job.status] ||
    phases.find((p) => p.value === phase(job))?.label ||
    job.status
  );
}
export function bytes(value: number | null = 0) {
  value = value || 0;
  if (value < 1024) return `${value} B`;
  const i = Math.min(3, Math.floor(Math.log(value) / Math.log(1024)));
  return `${(value / 1024 ** i).toFixed(1)} ${["B", "KiB", "MiB", "GiB"][i]}`;
}
export const pathLabel = (storage: Storage, path: string) =>
  `${storageNames[storage]} · /${path.replace(/^\//, "")}`;
export const time = (epoch: number) =>
  new Date(epoch * 1000).toLocaleString("zh-CN", { hour12: false });
export const outputName = (name: string, mode: string) => {
  const dot = name.lastIndexOf(".");
  return `${dot < 0 ? name : name.slice(0, dot)}.${mode === "replace" ? "translated" : "bilingual"}${dot < 0 ? "" : name.slice(dot)}`;
};
export const errorText = (error: unknown) =>
  error instanceof Error ? error.message : "请求失败，请重试。";
