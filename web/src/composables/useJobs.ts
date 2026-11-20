import {
  reactive,
  ref,
  computed,
  watch,
  nextTick,
  onMounted,
  onUnmounted,
} from "vue";
import { useQuery, useMutation, useQueryClient } from "@tanstack/vue-query";
import { api } from "../lib/api";
import { errorText, phase } from "../lib/format";
import type { Job, JobPage, Phase } from "../lib/types";

const MAX_BY_ID_ENTRIES = 200;

type JobStreamEvent =
  { type: "updated"; job: Job } | { type: "deleted"; id: string };

export function useJobs() {
  const client = useQueryClient();
  const selectedPhase = ref<Phase>("all");
  const cursors = ref<(string | null)[]>([null]);
  const sseConnected = ref(false);
  const listKey = computed(() => [
    "jobs",
    "list",
    selectedPhase.value,
    cursors.value.at(-1),
  ]);
  const isFirstAllPage = computed(
    () => selectedPhase.value === "all" && cursors.value.at(-1) === null,
  );

  const list = useQuery({
    queryKey: listKey,
    queryFn: ({ signal, queryKey }) => {
      const query = new URLSearchParams();
      if (queryKey[2] !== "all") query.set("phase", queryKey[2]!);
      if (queryKey[3]) query.set("cursor", queryKey[3]);
      return api<JobPage>(`/api/jobs?${query}`, { signal });
    },
    // Reconcile even jobs that finish between active polls; slow down when SSE is live.
    refetchInterval: (query) => {
      if (query.state.status === "error") return 2500;
      return sseConnected.value ? 30000 : 10000;
    },
  });
  const recent = useQuery({
    queryKey: ["jobs", "list", "all", null],
    queryFn: ({ signal }) => api<JobPage>("/api/jobs?", { signal }),
    enabled: computed(() => !isFirstAllPage.value),
    refetchInterval: (query) => {
      if (query.state.status === "error") return 2500;
      return sseConnected.value ? 30000 : 10000;
    },
  });
  const active = useQuery({
    queryKey: ["jobs", "active"],
    queryFn: ({ signal }) =>
      api<{ jobs: Job[] }>("/api/jobs/active", { signal }),
    refetchInterval: (query) => {
      if (query.state.status === "error") return 2500;
      const hasActive = (query.state.data?.jobs.length ?? 0) > 0;
      if (sseConnected.value) return hasActive ? 15000 : 30000;
      return hasActive ? 2500 : 10000;
    },
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
      const source = isFirstAllPage.value
        ? list.data.value?.jobs
        : recent.data.value?.jobs;
      return (source ?? []).slice(0, 3).map((job) => live.get(job.id) ?? job);
    },
    byId: {} as Record<string, Job>,
    get recentLoaded() {
      return isFirstAllPage.value ? !!list.data.value : !!recent.data.value;
    },
    get recentError() {
      const baseError = isFirstAllPage.value
        ? list.error.value
        : recent.error.value;
      const error = baseError || active.error.value;
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

  function rememberJobs(items: Job[]) {
    for (const job of items) state.byId[job.id] = job;
    const keys = Object.keys(state.byId);
    if (keys.length > MAX_BY_ID_ENTRIES) {
      const keep = new Set([
        ...(list.data.value?.jobs.map((j) => j.id) ?? []),
        ...(active.data.value?.jobs.map((j) => j.id) ?? []),
      ]);
      for (const id of keys) {
        if (!keep.has(id) && Object.keys(state.byId).length > MAX_BY_ID_ENTRIES)
          delete state.byId[id];
      }
    }
  }

  function applyStreamEvent(event: JobStreamEvent) {
    if (event.type === "deleted") {
      delete state.byId[event.id];
      void client.invalidateQueries({ queryKey: ["jobs"] });
      return;
    }
    const job = event.job;
    const previous = state.byId[job.id];
    rememberJobs([job]);
    const isTerminal = ["completed", "failed", "cancelled"].includes(
      job.status,
    );
    client.setQueryData<{ jobs: Job[] }>(["jobs", "active"], (current) => {
      if (!current) return current;
      const without = current.jobs.filter((item) => item.id !== job.id);
      return { jobs: isTerminal ? without : [job, ...without] };
    });
    if (!previous || phase(previous) !== phase(job) || isTerminal) {
      void client.invalidateQueries({ queryKey: ["jobs", "list"] });
    }
  }

  let eventSource: EventSource | null = null;
  onMounted(() => {
    if (typeof EventSource === "undefined") return;
    try {
      eventSource = new EventSource("/api/jobs/events");
      eventSource.onopen = () => {
        sseConnected.value = true;
      };
      eventSource.onerror = () => {
        sseConnected.value = false;
      };
      eventSource.addEventListener("job", (message) => {
        try {
          applyStreamEvent(JSON.parse((message as MessageEvent).data));
        } catch {
          // Ignore malformed event payloads.
        }
      });
      eventSource.addEventListener("resync", () => {
        void client.invalidateQueries({ queryKey: ["jobs"] });
      });
    } catch {
      sseConnected.value = false;
    }
  });
  onUnmounted(() => {
    eventSource?.close();
    eventSource = null;
    sseConnected.value = false;
  });

  watch(list.data, (page) => {
    if (!page) return;
    if (!page.jobs.length && cursors.value.length > 1) cursors.value.pop();
    rememberJobs(page.jobs);
  });
  watch(recent.data, (page) => {
    if (page?.jobs) rememberJobs(page.jobs);
  });
  watch(active.data, (current, previous) => {
    if (!current) return;
    rememberJobs(current.jobs);
    const live = new Map(current.jobs.map((job) => [job.id, job]));
    const changed = previous?.jobs.some(
      (job) => !live.has(job.id) || phase(job) !== phase(live.get(job.id)!),
    );
    const appeared = current.jobs.some(
      (job) => !previous?.jobs.some((old) => old.id === job.id),
    );
    const missed = list.data.value?.jobs.some(
      (job) =>
        !["completed", "failed", "cancelled"].includes(job.status) &&
        !live.has(job.id),
    );
    if (changed || appeared || missed)
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
