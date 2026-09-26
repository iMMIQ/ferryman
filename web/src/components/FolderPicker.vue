<script setup lang="ts">
import { ref, computed, watch } from "vue";
import type { Source, Storage, Entry } from "../lib/types";
import { storageNames, pathLabel, bytes } from "../lib/format";
import { useFolder } from "../composables/useFolder";
import AppDialog from "./ui/AppDialog.vue";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
import EmptyState from "./ui/EmptyState.vue";
const props = defineProps<{
  open: boolean;
  purpose: "source" | "save";
  selections: Source[];
  save?: Source | null;
}>();
const emit = defineEmits<{
  "update:open": [value: boolean];
  select: [value: Source[]];
  save: [value: Source];
}>();
const folder = useFolder(),
  s = folder.state,
  search = ref(""),
  kind = ref("all"),
  draft = ref<Source[]>([]),
  newFolder = ref(false),
  name = ref("");
watch(
  () => props.open,
  (open) => {
    if (open) {
      draft.value = props.selections.map((s) => ({ ...s }));
      search.value = "";
      kind.value = "all";
      newFolder.value = false;
      name.value = "";
      const storage =
        props.purpose === "save" && props.save ? props.save.storage : s.storage;
      void folder.load(
        storage,
        props.purpose === "save" && props.save
          ? props.save.path
          : s.paths[storage],
      );
    } else folder.close();
  },
);
const rows = computed(() => {
  const entries =
    props.purpose === "source"
      ? [
          {
            name: `当前目录 · ${s.path.split("/").at(-1) || "根目录"}`,
            path: s.path,
            kind: "directory" as const,
            size: null,
            supported: true,
          },
          ...s.entries,
        ]
      : s.entries.filter((e) => e.kind === "directory");
  return entries.filter(
    (e) =>
      (kind.value === "all" || e.kind === kind.value) &&
      `${e.name} ${e.path}`
        .toLocaleLowerCase()
        .includes(search.value.toLocaleLowerCase()),
  );
});
function selected(path: string) {
  return draft.value.some((x) => x.storage === s.storage && x.path === path);
}
function toggle(entry: Entry) {
  if (selected(entry.path))
    draft.value = draft.value.filter(
      (x) => !(x.storage === s.storage && x.path === entry.path),
    );
  else draft.value.push({ storage: s.storage, path: entry.path });
}
function navigate(storage: Storage, path: string) {
  search.value = "";
  newFolder.value = false;
  void folder.load(storage, path);
}
function confirm() {
  if (s.loading || s.error) return;
  if (props.purpose === "source")
    emit(
      "select",
      draft.value.map((x) => ({ ...x })),
    );
  else emit("save", { storage: s.storage, path: s.path });
  emit("update:open", false);
}
async function create() {
  if (await folder.create(name.value)) {
    newFolder.value = false;
    name.value = "";
  }
}
</script>
<template>
  <AppDialog
    id="folder-dialog"
    :open="open"
    :title="purpose === 'source' ? '选择文件或目录' : '选择保存目录'"
    description="切换存储位置浏览文件。来源可多选，保存位置为当前目录。"
    :busy="s.creating"
    @update:open="$emit('update:open', $event)"
  >
    <div
      id="folder-storage"
      class="filter-bar"
      role="group"
      aria-label="存储位置"
    >
      <button
        v-for="(label, key) in storageNames"
        :key="key"
        :data-folder-storage="key"
        :aria-pressed="s.storage === key"
        :disabled="s.creating"
        @click="navigate(key, s.paths[key])"
      >
        {{ label }}
      </button>
    </div>
    <div class="folder-location">
      <AppButton
        id="folder-up"
        aria-label="上一级"
        :disabled="s.loading || s.creating || s.parent == null"
        @click="navigate(s.storage, s.parent || '')"
        >↑</AppButton
      ><strong id="folder-current-path">{{
        s.loading ? "正在读取…" : pathLabel(s.storage, s.path)
      }}</strong
      ><AppButton
        v-if="purpose === 'save'"
        id="show-new-folder"
        :disabled="s.loading || s.creating || !!s.error"
        @click="newFolder = true"
        >新建文件夹</AppButton
      >
    </div>
    <div class="folder-filters">
      <input
        id="folder-search-input"
        v-model="search"
        type="search"
        placeholder="过滤当前目录"
        aria-label="过滤当前目录"
      />
      <div class="filter-bar" role="group" aria-label="内容类型">
        <button
          v-for="option in [
            { value: 'all', label: '全部' },
            { value: 'directory', label: '目录' },
            { value: 'file', label: '文件' },
          ]"
          :key="option.value"
          :aria-pressed="kind === option.value"
          @click="kind = option.value"
        >
          {{ option.label }}
        </button>
      </div>
    </div>
    <form
      v-if="newFolder"
      id="new-folder-form"
      class="inline-form"
      @submit.prevent="create"
    >
      <input
        id="new-folder-name"
        v-model="name"
        aria-label="文件夹名称"
        maxlength="80"
        required
        :disabled="s.creating"
      /><AppButton type="submit" variant="primary" :busy="s.creating"
        >创建</AppButton
      ><AppButton :disabled="s.creating" @click="newFolder = false"
        >取消</AppButton
      >
    </form>
    <AppNotice :message="s.error"
      ><AppButton @click="folder.load(s.storage, s.paths[s.storage])"
        >重试</AppButton
      ></AppNotice
    >
    <p v-if="s.loading" role="status">正在读取…</p>
    <ul
      v-else-if="!s.error"
      id="folder-list"
      class="folder-list"
      aria-label="文件与目录"
    >
      <li
        v-for="entry in rows"
        :key="`${s.storage}:${entry.path}`"
        class="folder-row"
        :class="{ selected: selected(entry.path) }"
      >
        <button
          v-if="purpose === 'source'"
          class="folder-checkbox"
          :data-entry-select="entry.path"
          :aria-label="`${selected(entry.path) ? '取消选择' : '选择'} ${entry.name}`"
          :aria-pressed="selected(entry.path)"
          :disabled="entry.kind === 'file' && !entry.supported"
          @click="toggle(entry)"
        >
          {{ selected(entry.path) ? "✓" : "" }}
        </button>
        <span aria-hidden="true">{{
          entry.kind === "directory" ? "▰" : "▤"
        }}</span
        ><button
          class="folder-row-main"
          :disabled="entry.kind === 'file' && !entry.supported"
          @click="
            entry.kind === 'directory' && entry.path !== s.path
              ? navigate(s.storage, entry.path)
              : toggle(entry)
          "
        >
          <strong>{{ entry.name }}</strong
          ><span>{{
            entry.kind === "directory"
              ? entry.path === s.path
                ? "选择整个目录"
                : "打开目录"
              : entry.supported
                ? bytes(entry.size)
                : "不支持"
          }}</span>
        </button>
      </li>
    </ul>
    <EmptyState
      v-if="!s.loading && !s.error && !rows.length"
      title="没有匹配项"
      description="尝试其他目录或清除筛选条件。"
    />
    <template #footer
      ><div v-if="purpose === 'source'" class="selection-info">
        <span>已选 {{ draft.length }} 项</span
        ><AppButton variant="ghost" @click="draft = []">清空</AppButton>
      </div>
      <AppButton
        id="cancel-folder-dialog"
        :disabled="s.creating"
        @click="$emit('update:open', false)"
        >取消</AppButton
      ><AppButton
        id="select-current-folder"
        variant="primary"
        :disabled="
          s.loading ||
          s.creating ||
          !!s.error ||
          (purpose === 'source' && !draft.length)
        "
        @click="confirm"
        >{{ purpose === "source" ? "确认选择" : "保存到此目录" }}</AppButton
      ></template
    >
  </AppDialog>
</template>
