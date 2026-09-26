import { reactive, onUnmounted } from "vue";
import { api, json, latestQuery, isCancelled } from "../lib/api";
import type { Listing, Storage, Entry } from "../lib/types";
import { errorText } from "../lib/format";
export function useFolder() {
  const state = reactive({
    storage: "documents" as Storage,
    path: "",
    parent: null as string | null,
    entries: [] as Entry[],
    loading: false,
    error: "",
    creating: false,
    paths: { documents: "", remote_fs: "" },
  });
  const query = latestQuery();
  async function load(storage: Storage, path: string) {
    const ticket = query.begin();
    state.storage = storage;
    state.loading = true;
    state.entries = [];
    state.error = "";
    try {
      const data = await api<Listing>(
        `/api/documents?${new URLSearchParams({ storage, path })}`,
        { signal: ticket.signal },
      );
      if (!ticket.current()) return;
      state.path = data.path;
      state.parent = data.parent;
      state.entries = data.entries;
      state.paths[storage] = data.path;
    } catch (error) {
      if (ticket.current() && !isCancelled(error))
        state.error = errorText(error);
    } finally {
      if (ticket.current()) state.loading = false;
    }
  }
  async function create(name: string) {
    if (state.creating || state.loading) return false;
    if (
      !name.trim() ||
      /[\\/]/.test(name) ||
      [".", ".."].includes(name.trim())
    ) {
      state.error = "请输入有效的文件夹名称";
      return false;
    }
    const storage = state.storage,
      path = state.path ? `${state.path}/${name.trim()}` : name.trim();
    state.creating = true;
    state.error = "";
    try {
      await api("/api/documents/directories", json({ storage, path }));
      await load(storage, path);
      return true;
    } catch (error) {
      state.error = errorText(error);
      return false;
    } finally {
      state.creating = false;
    }
  }
  function close() {
    query.cancel();
    state.loading = false;
  }
  onUnmounted(close);
  return { state, load, create, close };
}
