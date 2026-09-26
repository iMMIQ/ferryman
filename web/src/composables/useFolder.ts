import { reactive, ref, computed, watch, nextTick } from "vue";
import { useQuery, useMutation, useQueryClient } from "@tanstack/vue-query";
import { api, json } from "../lib/api";
import type { Listing, Storage } from "../lib/types";
import { errorText } from "../lib/format";
export function useFolder() {
  const client = useQueryClient();
  const location = ref<{ storage: Storage; path: string }>({
    storage: "documents",
    path: "",
  });
  const opened = ref(false),
    actionError = ref("");
  const key = computed(() => [
    "documents",
    location.value.storage,
    location.value.path,
  ]);
  const query = useQuery({
    queryKey: key,
    enabled: opened,
    queryFn: ({ signal, queryKey }) =>
      api<Listing>(
        `/api/documents?${new URLSearchParams({ storage: queryKey[1], path: queryKey[2] })}`,
        { signal },
      ),
  });
  const createFolder = useMutation({
    mutationFn: (folder: { storage: Storage; path: string }) =>
      api("/api/documents/directories", json(folder)),
    onSuccess: () => client.invalidateQueries({ queryKey: ["documents"] }),
  });
  const state = reactive({
    get storage() {
      return location.value.storage;
    },
    get path() {
      return query.data.value?.path ?? location.value.path;
    },
    get parent() {
      return query.data.value?.parent ?? null;
    },
    get entries() {
      return query.data.value?.entries ?? [];
    },
    get loading() {
      return opened.value && query.isFetching.value;
    },
    get error() {
      return (
        actionError.value ||
        (query.error.value ? errorText(query.error.value) : "")
      );
    },
    get creating() {
      return createFolder.isPending.value;
    },
    paths: { documents: "", remote_fs: "" },
  });
  watch(query.data, (data) => {
    if (data) state.paths[location.value.storage] = data.path;
  });
  async function load(storage: Storage, path: string) {
    actionError.value = "";
    location.value = { storage, path };
    opened.value = true;
    await nextTick();
    await query.refetch({ cancelRefetch: false });
  }
  async function create(name: string) {
    if (state.creating || state.loading) return false;
    if (
      !name.trim() ||
      /[\\/]/.test(name) ||
      [".", ".."].includes(name.trim())
    ) {
      actionError.value = "请输入有效的文件夹名称";
      return false;
    }
    const storage = state.storage,
      path = state.path ? `${state.path}/${name.trim()}` : name.trim();
    actionError.value = "";
    try {
      await createFolder.mutateAsync({ storage, path });
      await load(storage, path);
      return true;
    } catch (error) {
      actionError.value = errorText(error);
      return false;
    }
  }
  function close() {
    opened.value = false;
    void client.cancelQueries({ queryKey: key.value });
  }
  return { state, load, create, close };
}
