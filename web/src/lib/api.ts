import { isCancelledError } from "@tanstack/vue-query";
export class ApiError extends Error {
  constructor(
    message: string,
    public kind: "http" | "network" | "timeout" | "contract",
    public status?: number,
  ) {
    super(message);
  }
}
export const isCancelled = (error: unknown) =>
  isCancelledError(error) ||
  (error instanceof DOMException && error.name === "AbortError");
export async function api<T>(
  path: string,
  options: RequestInit = {},
  timeoutMs = 20000,
): Promise<T> {
  const controller = new AbortController();
  let timedOut = false;
  const abort = () => controller.abort();
  if (options.signal?.aborted) controller.abort();
  options.signal?.addEventListener("abort", abort, { once: true });
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  try {
    const response = await fetch(path, {
      ...options,
      signal: controller.signal,
    });
    const content = response.headers.get("content-type") || "";
    let body = null;
    if (content.includes("application/json")) {
      try {
        body = await response.json();
      } catch {
        throw new ApiError("服务返回了无效的 JSON 数据。", "contract");
      }
    }
    if (!response.ok)
      throw new ApiError(
        body?.error || `请求失败 (${response.status})`,
        "http",
        response.status,
      );
    if (response.status !== 204 && body === null)
      throw new ApiError("服务返回了非预期的数据，请刷新后重试。", "contract");
    return body as T;
  } catch (error) {
    if (timedOut) throw new ApiError("请求超时，请重试。", "timeout");
    if (controller.signal.aborted)
      throw new DOMException("请求已取消", "AbortError");
    if (error instanceof ApiError) throw error;
    throw new ApiError("连接中断，请检查网络后重试。", "network");
  } finally {
    clearTimeout(timer);
    options.signal?.removeEventListener("abort", abort);
  }
}
export const json = (body: unknown): RequestInit => ({
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(body),
});
