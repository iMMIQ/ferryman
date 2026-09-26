import { reactive, onUnmounted } from "vue";
import { api, latestQuery, isCancelled } from "../lib/api";
import { errorText, phase } from "../lib/format";
import type { Job, JobPage, Phase } from "../lib/types";
import { usePolling } from "./usePolling";
export function useJobs() {
  const state = reactive({
    jobs: [] as Job[],
    byId: {} as Record<string, Job>,
    loaded: false,
    loading: false,
    error: "",
    updated: "",
    total: 0,
    phase: "all" as Phase,
    cursors: [null] as (string | null)[],
    next: null as string | null,
    active: 0,
    pending: {} as Record<string, boolean>,
    errors: {} as Record<string, string>,
  });
  const listQuery = latestQuery(),
    activeQuery = latestQuery();
  let activeFlight: Promise<void> | undefined,
    previous = new Map<string, Job>();
  function remember(jobs: Job[]) {
    for (const job of jobs) state.byId[job.id] = job;
  }
  async function refresh() {
    const ticket = listQuery.begin();
    state.loading = true;
    const query = new URLSearchParams();
    const cursor = state.cursors.at(-1);
    if (cursor) query.set("cursor", cursor);
    if (state.phase !== "all") query.set("phase", state.phase);
    try {
      const page = await api<JobPage>(`/api/jobs?${query}`, {
        signal: ticket.signal,
      });
      if (!ticket.current()) return;
      if (!Array.isArray(page.jobs)) throw new Error("任务列表格式无效");
      if (!page.jobs.length && state.cursors.length > 1) {
        state.cursors.pop();
        return await refresh();
      }
      state.jobs = page.jobs;
      remember(page.jobs);
      state.total = page.total;
      state.next = page.next_cursor || null;
      state.loaded = true;
      state.error = "";
      state.updated = new Date().toLocaleTimeString("zh-CN");
    } catch (error) {
      if (ticket.current() && !isCancelled(error))
        state.error = errorText(error);
    } finally {
      if (ticket.current()) state.loading = false;
    }
  }
  function refreshActive() {
    if (activeFlight) return activeFlight;
    activeFlight = (async () => {
      const ticket = activeQuery.begin();
      try {
        const response = await api<{ jobs: Job[] }>("/api/jobs/active", {
          signal: ticket.signal,
        });
        if (!ticket.current()) return;
        const active = new Map(response.jobs.map((job) => [job.id, job]));
        const changed =
          [...previous.keys()].some((id) => !active.has(id)) ||
          response.jobs.some(
            (job) =>
              previous.has(job.id) &&
              phase(previous.get(job.id)!) !== phase(job),
          );
        const appeared =
          state.cursors.length === 1 &&
          response.jobs.some(
            (job) =>
              (state.phase === "all" || phase(job) === state.phase) &&
              !state.jobs.some((j) => j.id === job.id),
          );
        previous = active;
        state.active = active.size;
        remember(response.jobs);
        if (!state.loaded || state.error || changed || appeared)
          await refresh();
        else state.jobs = state.jobs.map((job) => active.get(job.id) || job);
      } catch (error) {
        if (ticket.current() && !isCancelled(error))
          state.error = errorText(error);
      }
    })().finally(() => {
      activeFlight = undefined;
    });
    return activeFlight;
  }
  async function first() {
    state.cursors = [null];
    await refresh();
  }
  async function filter(value: Phase) {
    if (state.phase === value) return;
    state.phase = value;
    await first();
  }
  async function next() {
    if (!state.next || state.loading) return;
    state.cursors.push(state.next);
    await refresh();
  }
  async function previousPage() {
    if (state.cursors.length < 2 || state.loading) return;
    state.cursors.pop();
    await refresh();
  }
  async function act(id: string, action: "retry" | "cancel" | "delete") {
    if (state.pending[id]) return false;
    state.pending[id] = true;
    state.errors[id] = "";
    try {
      await api(`/api/jobs/${id}${action === "delete" ? "" : `/${action}`}`, {
        method: action === "delete" ? "DELETE" : "POST",
      });
      if (action === "delete") delete state.byId[id];
      activeQuery.cancel();
      if (activeFlight) await activeFlight;
      await refresh();
      await refreshActive();
      return true;
    } catch (error) {
      state.errors[id] = errorText(error);
      return false;
    } finally {
      state.pending[id] = false;
    }
  }
  usePolling(async () => {
    // Load history independently of the live-progress endpoint.
    if (!state.loaded) await refresh();
    if (state.loaded) await refreshActive();
  });
  onUnmounted(() => {
    listQuery.cancel();
    activeQuery.cancel();
  });
  return { state, refresh, first, filter, next, previousPage, act };
}
export type JobsStore = ReturnType<typeof useJobs>;
