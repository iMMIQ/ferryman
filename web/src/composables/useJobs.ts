import { reactive, ref, computed, watch, nextTick } from "vue";
import { useQuery, useMutation, useQueryClient } from "@tanstack/vue-query";
import { api } from "../lib/api";
import { errorText, phase } from "../lib/format";
import type { Job, JobPage, Phase } from "../lib/types";
export function useJobs() {
  const client = useQueryClient();
  const selectedPhase = ref<Phase>("all");
  const cursors = ref<(string | null)[]>([null]);
  const listKey = computed(() => [
    "jobs",
    "list",
    selectedPhase.value,
    cursors.value.at(-1),
  ]);
  const list = useQuery({
    queryKey: listKey,
    queryFn: ({ signal, queryKey }) => {
      const query = new URLSearchParams();
      if (queryKey[2] !== "all") query.set("phase", queryKey[2]!);
      if (queryKey[3]) query.set("cursor", queryKey[3]);
      return api<JobPage>(`/api/jobs?${query}`, { signal });
    },
    // History only refreshes on invalidation; failed initial loads can recover.
    refetchInterval: (query) => (query.state.status === "error" ? 2500 : false),
  });
  const recent = useQuery({
    queryKey: ["jobs", "list", "all", null],
    queryFn: ({ signal }) => api<JobPage>("/api/jobs?", { signal }),
    refetchInterval: (query) => (query.state.status === "error" ? 2500 : false),
  });
  const active = useQuery({
    queryKey: ["jobs", "active"],
    queryFn: ({ signal }) =>
      api<{ jobs: Job[] }>("/api/jobs/active", { signal }),
    refetchInterval: 2500,
  });
  const state = reactive({
    get jobs() {
      const live = new Map(
        active.data.value?.jobs.map((job) => [job.id, job]) ?? [],
      );
      return (list.data.value?.jobs ?? []).map(
        (job) => live.get(job.id) ?? job,
      );
    },
    get recent() {
      const live = new Map(
        active.data.value?.jobs.map((job) => [job.id, job]) ?? [],
      );
      return (recent.data.value?.jobs ?? [])
        .slice(0, 3)
        .map((job) => live.get(job.id) ?? job);
    },
    byId: {} as Record<string, Job>,
    get recentLoaded() {
      return !!recent.data.value;
    },
    get recentError() {
      const error = recent.error.value || active.error.value;
      return error ? errorText(error) : "";
    },
    get loaded() {
      return !!list.data.value;
    },
    get loading() {
      return list.isFetching.value;
    },
    get error() {
      const error = list.error.value || active.error.value;
      return error ? errorText(error) : "";
    },
    get updated() {
      return list.dataUpdatedAt.value
        ? new Date(list.dataUpdatedAt.value).toLocaleTimeString("zh-CN")
        : "";
    },
    get total() {
      return list.data.value?.total ?? 0;
    },
    get phase() {
      return selectedPhase.value;
    },
    get cursors() {
      return cursors.value;
    },
    get next() {
      return list.data.value?.next_cursor ?? null;
    },
    get active() {
      return active.data.value?.jobs.length ?? 0;
    },
    pending: {} as Record<string, boolean>,
    errors: {} as Record<string, string>,
  });
  watch(list.data, (page) => {
    if (!page) return;
    if (!page.jobs.length && cursors.value.length > 1) cursors.value.pop();
    for (const job of page.jobs) state.byId[job.id] = job;
  });
  watch(recent.data, (page) => {
    for (const job of page?.jobs ?? []) state.byId[job.id] = job;
  });
  watch(active.data, (current, previous) => {
    if (!current) return;
    for (const job of current.jobs) state.byId[job.id] = job;
    const live = new Map(current.jobs.map((job) => [job.id, job]));
    const changed = previous?.jobs.some(
      (job) => !live.has(job.id) || phase(job) !== phase(live.get(job.id)!),
    );
    const appeared = current.jobs.some(
      (job) => !previous?.jobs.some((old) => old.id === job.id),
    );
    if (changed || appeared)
      void client.invalidateQueries({ queryKey: ["jobs", "list"] });
  });
  const mutation = useMutation({
    mutationFn: ({
      id,
      action,
    }: {
      id: string;
      action: "retry" | "cancel" | "delete";
    }) =>
      api(`/api/jobs/${id}${action === "delete" ? "" : `/${action}`}`, {
        method: action === "delete" ? "DELETE" : "POST",
      }),
    onSuccess: async () => {
      await client.cancelQueries({ queryKey: ["jobs"] });
      await client.invalidateQueries({ queryKey: ["jobs"] });
    },
  });
  async function refresh() {
    await client.refetchQueries({ queryKey: ["jobs"], type: "active" });
  }
  async function first() {
    selectedPhase.value = "all";
    cursors.value = [null];
    await nextTick();
    await refresh();
  }
  async function filter(value: Phase) {
    if (selectedPhase.value === value) return;
    selectedPhase.value = value;
    cursors.value = [null];
  }
  async function next() {
    if (state.next && !state.loading) cursors.value.push(state.next);
  }
  async function previousPage() {
    if (cursors.value.length > 1 && !state.loading) cursors.value.pop();
  }
  async function act(id: string, action: "retry" | "cancel" | "delete") {
    if (state.pending[id]) return false;
    state.pending[id] = true;
    state.errors[id] = "";
    try {
      await mutation.mutateAsync({ id, action });
      if (action === "delete") delete state.byId[id];
      return true;
    } catch (error) {
      state.errors[id] = errorText(error);
      return false;
    } finally {
      state.pending[id] = false;
    }
  }
  return { state, refresh, first, filter, next, previousPage, act };
}
export type JobsStore = ReturnType<typeof useJobs>;
