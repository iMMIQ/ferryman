<script setup lang="ts">
import { ref, computed, watch } from "vue";
import type { RuntimeStore } from "../composables/useRuntime";
import { useSubmission } from "../composables/useSubmission";
import { bytes, pathLabel, outputName, modelStates } from "../lib/format";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
import AppDialog from "./ui/AppDialog.vue";
import SegmentedControl from "./ui/SegmentedControl.vue";
import FolderPicker from "./FolderPicker.vue";
const props = defineProps<{
  runtime: RuntimeStore;
  completed: () => Promise<void>;
}>();
const submission = useSubmission(async (preset) => {
  const model = props.runtime.state.catalog.models.find(
    (m) => m.preset === preset,
  );
  if (
    !model ||
    !["ready", "benchmarking", "downloading", "verifying"].includes(model.state)
  )
    await props.runtime.download(preset);
}, props.completed);
const { s, locked, hasDocx, summary } = submission;
const pickerOpen = ref(false),
  pickerPurpose = ref<"source" | "save">("source"),
  fileInput = ref<HTMLInputElement>(),
  dragging = ref(false);
watch(
  () => s.sourceMode,
  () => {
    if (hasDocx.value) s.mode = "bilingual";
    s.validation = "";
  },
);
const saveHint = computed(() =>
  s.sourceMode === "mounted" && s.strategy === "sibling_overwrite"
    ? "覆盖原文件：提交前将列出受影响文件并要求确认。"
    : `保存副本，例如 ${outputName(s.files[0]?.name || "book.epub", s.mode)}。${s.save && (s.sourceMode === "upload" || s.strategy === "directory") ? pathLabel(s.save.storage, s.save.path) : s.sourceMode === "upload" ? "完成后下载" : "与来源文件放在同一目录"}`,
);
const selectedModel = computed(() =>
  props.runtime.state.catalog.models.find((m) => m.preset === s.preset),
);
const overwrite = computed(() =>
  s.preview?.files.some((f) => f.overwrite && !f.skip_reason),
);
function pick(purpose: "source" | "save") {
  if (locked.value) return;
  pickerPurpose.value = purpose;
  pickerOpen.value = true;
}
function filesChanged(event: Event) {
  const input = event.target as HTMLInputElement;
  submission.add(Array.from(input.files || []));
  input.value = "";
}
function drop(event: DragEvent) {
  dragging.value = false;
  submission.add(Array.from(event.dataTransfer?.files || []));
}
function chooseFile() {
  if (locked.value) return;
  s.sourceMode = "upload";
  fileInput.value?.click();
}
defineExpose({ chooseFile });
</script>
<template>
  <aside class="submit-panel">
    <span class="eyebrow">新任务</span>
    <h2>创建翻译</h2>
    <form
      id="job-form"
      :aria-busy="locked"
      @submit.prevent="submission.preview"
    >
      <fieldset class="draft-fields" :disabled="locked">
        <SegmentedControl
          v-model="s.sourceMode"
          name="source"
          label="翻译来源"
          :options="[
            { value: 'upload', label: '上传文件' },
            { value: 'mounted', label: '文稿与网盘' },
          ]"
        />
        <div v-show="s.sourceMode === 'upload'">
          <label
            id="drop-zone"
            class="drop-zone"
            :class="{ dragging }"
            @dragover.prevent="dragging = !locked"
            @dragleave.prevent="dragging = false"
            @drop.prevent="drop"
            ><input
              id="file-input"
              ref="fileInput"
              type="file"
              multiple
              accept=".epub,.docx,.pdf,.srt,.vtt,.ass,.ssa,.lrc,.txt,.md,.markdown"
              @change="filesChanged"
            /><span aria-hidden="true" class="upload-icon">↑</span
            ><strong>选择或拖入文档</strong
            ><span>EPUB、DOCX、PDF、字幕、TXT、Markdown</span></label
          >
          <p id="upload-hint" class="helper-text">
            支持多文件 · 单文件最大 {{ bytes(s.maxBytes) }}
          </p>
          <ul id="upload-list" class="upload-list" aria-label="待上传文件">
            <li
              v-for="(file, index) in s.files"
              :key="`${file.name}:${file.size}:${file.lastModified}`"
            >
              <span
                >{{ file.name }}<small>{{ bytes(file.size) }}</small></span
              ><AppButton
                variant="ghost"
                :data-remove-file="index"
                :aria-label="`移除 ${file.name}`"
                @click="s.files.splice(index, 1)"
                >×</AppButton
              >
            </li>
          </ul>
          <AppNotice id="upload-validation" :message="s.validation" />
        </div>
        <AppButton
          v-if="s.sourceMode === 'mounted'"
          id="source-directory"
          class="path-picker"
          @click="pick('source')"
          ><span
            >来源文件与目录<strong>{{
              s.sources.length
                ? `已选 ${s.sources.length} 项`
                : "选择文件或目录"
            }}</strong></span
          ><span aria-hidden="true">›</span></AppButton
        >
        <label class="field-label" for="target-language">目标语言</label
        ><input
          id="target-language"
          v-model="s.target"
          name="target"
          list="target-languages"
          maxlength="64"
          required
          autocomplete="off"
        /><datalist id="target-languages">
          <option
            v-for="language in [
              '中文',
              'English',
              '日本語',
              '한국어',
              'Français',
              'Deutsch',
              'Español',
            ]"
            :key="language"
            :value="language"
          />
        </datalist>
        <SegmentedControl
          v-model="s.mode"
          name="mode"
          label="输出方式"
          :options="[
            { value: 'bilingual', label: '双语对照' },
            { value: 'replace', label: '仅译文', disabled: hasDocx },
          ]"
        />
        <p v-if="hasDocx" id="mode-hint" class="helper-text">
          DOCX 仅支持双语对照，已为你选择兼容的输出方式。
        </p>
        <details class="settings">
          <summary>
            翻译模型
            <span>{{
              s.preset === "7b-fp8" ? "7B · 默认" : "30B · 更大模型"
            }}</span>
          </summary>
          <SegmentedControl
            v-model="s.preset"
            name="preset"
            label="翻译模型"
            :options="[
              { value: '7b-fp8', label: '7B（默认）' },
              { value: '30b-fp8', label: '30B（更大模型）' },
            ]"
          />
          <p class="helper-text">
            {{
              selectedModel ? modelStates[selectedModel.state] : "状态待同步"
            }}。提交后自动准备模型，无需手动启动。
          </p>
        </details>
        <details class="settings">
          <summary>高级设置</summary>
          <div class="tuning-grid">
            <label
              >每批段数<input
                v-model.number="s.batch"
                name="batch_size"
                type="number"
                min="1"
                max="100"
                step="1"
                required /></label
            ><label
              >上下文段数<input
                v-model.number="s.context"
                name="context_segments"
                type="number"
                min="0"
                max="50"
                step="1"
                required
            /></label>
          </div>
          <label class="check-setting"
            ><input
              id="cache-enabled"
              v-model="s.cache"
              type="checkbox"
            />使用翻译缓存</label
          >
        </details>
        <SegmentedControl
          v-if="s.sourceMode === 'mounted'"
          v-model="s.strategy"
          name="save_strategy"
          label="保存方式"
          :options="[
            { value: 'sibling_suffix', label: '保存副本' },
            { value: 'sibling_overwrite', label: '覆盖原文件' },
            { value: 'directory', label: '统一目录' },
          ]"
        />
        <div v-if="s.sourceMode === 'upload' || s.strategy === 'directory'">
          <span class="field-label">保存位置</span>
          <div class="path-group">
            <AppButton
              id="save-directory"
              class="path-picker"
              @click="pick('save')"
              ><span
                >{{ s.save ? "保存到" : "任务内保存"
                }}<strong>{{
                  s.save ? pathLabel(s.save.storage, s.save.path) : "完成后下载"
                }}</strong></span
              ><span aria-hidden="true">›</span></AppButton
            ><AppButton
              v-if="s.save"
              id="clear-save-directory"
              aria-label="清除保存位置"
              @click="s.save = null"
              >×</AppButton
            >
          </div>
        </div>
        <p id="save-hint" class="helper-text">{{ saveHint }}</p>
      </fieldset>
      <AppNotice id="submission-error" :message="s.error" />
      <AppNotice id="submission-result" :message="s.result" tone="info" />
      <div
        v-if="s.stage === 'submitting'"
        id="upload-progress"
        class="upload-progress"
        role="status"
      >
        <label for="upload-meter"
          >{{ s.uploadLabel || "正在准备任务…" }}
          {{
            s.uploading && s.progress !== null ? `${s.progress}%` : ""
          }}</label
        ><progress
          v-if="s.uploading"
          id="upload-meter"
          :value="s.progress ?? undefined"
          max="100"
        /><AppButton
          v-if="s.sourceMode === 'upload'"
          id="cancel-upload"
          @click="submission.cancel"
          >取消上传</AppButton
        >
      </div>
      <div class="submit-bar">
        <span class="helper-text">提交前可预览文件与保存位置</span
        ><AppButton
          id="submit-job"
          type="submit"
          variant="primary"
          :busy="locked"
          >{{
            s.stage === "previewing"
              ? "正在预览…"
              : s.stage === "submitting"
                ? "正在提交…"
                : "＋ 预览并提交"
          }}</AppButton
        ><AppButton
          v-if="s.stage === 'previewing'"
          id="cancel-preview"
          @click="submission.edit"
          >取消预览</AppButton
        >
      </div>
    </form>
    <FolderPicker
      v-model:open="pickerOpen"
      :purpose="pickerPurpose"
      :selections="s.sources"
      :save="s.save"
      @select="s.sources = $event"
      @save="s.save = $event"
    />
    <AppDialog
      id="submission-dialog"
      :open="s.stage === 'confirming'"
      title="确认翻译任务"
      description="未支持的格式与已生成的译文不加入任务。实际提交时会重新检查文件和保存位置。"
      @update:open="
        (value) => {
          if (!value) submission.edit();
        }
      "
    >
      <p id="submission-summary">{{ summary }}</p>
      <div id="submission-files" class="preview-files">
        <div
          v-for="(file, index) in s.preview?.files"
          :key="index"
          class="preview-file"
          :class="{ skipped: file.skip_reason }"
        >
          <strong>{{
            file.source_storage
              ? pathLabel(file.source_storage, file.source_path)
              : file.source_path
          }}</strong
          ><span>{{
            file.skip_reason
              ? `跳过：${file.skip_reason}`
              : `${file.overwrite ? "覆盖原文件" : "保存副本"} → ${file.save_storage ? pathLabel(file.save_storage, file.save_path) : `完成后下载 / ${file.save_path}`}`
          }}</span>
        </div>
      </div>
      <label v-if="overwrite" class="check-setting"
        ><input
          id="confirm-overwrite"
          v-model="s.acknowledged"
          type="checkbox"
        />我确认覆盖以上文件，原内容将被替换</label
      >
      <template #footer
        ><AppButton @click="submission.edit">返回修改</AppButton
        ><AppButton
          id="confirm-submit"
          variant="primary"
          :disabled="
            !s.preview?.eligible_count || (overwrite && !s.acknowledged)
          "
          @click="submission.submit"
          >确认加入队列</AppButton
        ></template
      >
    </AppDialog>
  </aside>
</template>
