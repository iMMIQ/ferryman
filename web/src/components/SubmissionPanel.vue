<script setup lang="ts">
import { ref, computed, watch, nextTick } from "vue";
import type { ModelsStore } from "../composables/useModels";
import { useSubmission } from "../composables/useSubmission";
import { bytes, pathLabel } from "../lib/format";
import AppIcon from "./ui/AppIcon.vue";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
import AppDialog from "./ui/AppDialog.vue";
import SegmentedControl from "./ui/SegmentedControl.vue";
import FolderPicker from "./FolderPicker.vue";
const props = defineProps<{
  models: ModelsStore;
  completed: (ids: string[], message: string) => Promise<void>;
}>();
defineEmits<{ "manage-models": [] }>();
const submission = useSubmission(props.completed);
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
const selectedModel = computed(() =>
  props.models.models.value.find((m) => m.target.id === s.model_id),
);
watch(
  () => props.models.models.value,
  (models) => {
    if (!s.model_id && !locked.value)
      s.model_id = models.find((m) => m.available)?.target.id || "";
  },
  { immediate: true },
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
async function removeFile(index: number) {
  const focused = document.activeElement?.matches("[data-remove-file]");
  s.files.splice(index, 1);
  await nextTick();
  if (focused) {
    const buttons = document.querySelectorAll<HTMLButtonElement>(
      "#upload-list li:not(.file-list-leave-active) [data-remove-file]",
    );
    (
      buttons[Math.min(index, buttons.length - 1)] ||
      document.querySelector<HTMLButtonElement>("#add-files")
    )?.focus();
  }
}
function revealInvalid(event: Event) {
  const details = (event.target as HTMLElement).closest("details");
  if (details) details.open = true;
}
defineExpose({ chooseFile });
</script>
<template>
  <section class="submit-panel" aria-label="新建翻译">
    <form
      id="job-form"
      :aria-busy="locked"
      @submit.prevent="submission.preview"
      @invalid.capture="revealInvalid"
    >
      <fieldset class="draft-fields" :disabled="locked">
        <section class="source-section">
          <div class="source-heading">
            <h2 class="form-section-title">
              {{
                s.sourceMode === "upload" && s.files.length
                  ? `${s.files.length} 个文件`
                  : "选择文件"
              }}
            </h2>
            <AppButton
              v-if="s.sourceMode === 'upload'"
              id="add-files"
              variant="ghost"
              @click="chooseFile"
              ><AppIcon name="plus" :size="16" />添加文件</AppButton
            >
          </div>
          <SegmentedControl
            v-model="s.sourceMode"
            name="source"
            label="翻译来源"
            :options="[
              { value: 'upload', label: '上传文件' },
              { value: 'mounted', label: '文稿与网盘' },
            ]"
          />
          <div
            v-show="s.sourceMode === 'upload'"
            class="upload-area"
            :class="{ dragging }"
            @dragover.prevent="dragging = !locked"
            @dragleave.prevent="dragging = false"
            @drop.prevent="drop"
          >
            <input
              id="file-input"
              ref="fileInput"
              type="file"
              multiple
              class="visually-hidden"
              accept=".epub,.docx,.pdf,.srt,.vtt,.ass,.ssa,.lrc,.txt,.md,.markdown"
              @change="filesChanged"
            />
            <label
              v-if="!s.files.length"
              id="drop-zone"
              for="file-input"
              class="drop-zone"
            >
              <AppIcon name="upload" :size="28" /><strong
                >拖入文档，或选择文件</strong
              >
            </label>
            <details v-if="!s.files.length" class="upload-help">
              <summary>支持格式与大小</summary>
              <p id="upload-hint">
                EPUB、DOCX、PDF、TXT、Markdown、字幕 · 单文件最大
                {{ bytes(s.maxBytes) }}
              </p>
            </details>
            <TransitionGroup
              tag="ul"
              name="file-list"
              id="upload-list"
              class="upload-list"
              aria-label="待上传文件"
              @before-leave="(element) => element.setAttribute('inert', '')"
            >
              <li
                v-for="(file, index) in s.files"
                :key="`${file.name}:${file.size}:${file.lastModified}`"
              >
                <span
                  class="file-type"
                  :data-type="file.name.split('.').at(-1)?.toLowerCase()"
                  aria-hidden="true"
                  ><AppIcon name="file"
                /></span>
                <span class="upload-filename"
                  >{{ file.name }}<small>{{ bytes(file.size) }}</small></span
                ><AppButton
                  variant="ghost"
                  :data-remove-file="index"
                  :aria-label="`移除 ${file.name}`"
                  @click="removeFile(index)"
                  >×</AppButton
                >
              </li>
            </TransitionGroup>
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
        </section>
        <section class="translation-section">
          <div class="translation-grid">
            <div>
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
            </div>
            <div>
              <label for="translation-preset" class="field-label"
                >翻译模型</label
              >
              <select id="translation-preset" v-model="s.model_id">
                <option value="" disabled>请选择模型</option>
                <option
                  v-for="model in models.models.value"
                  :key="model.target.id"
                  :value="model.target.id"
                  :disabled="!model.available"
                >
                  {{ model.target.name
                  }}{{ model.source === "custom" ? " · 自定义" : ""
                  }}{{ !model.available ? " · 不可用" : "" }}
                </option>
              </select>
              <AppButton
                v-if="models.loaded.value && !models.models.value.length"
                id="configure-models"
                variant="ghost"
                @click="$emit('manage-models')"
                >添加模型</AppButton
              >
            </div>
          </div>
          <AppNotice
            v-if="selectedModel && !selectedModel.available"
            message="所选模型当前不可用，请选择其他模型。"
          />
          <SegmentedControl
            v-model="s.mode"
            name="mode"
            class="output-options"
            label="输出方式"
            :options="[
              {
                value: 'bilingual',
                label: '双语对照',
              },
              {
                value: 'replace',
                label: '仅译文',
                disabled: hasDocx,
              },
            ]"
          />
          <p v-if="hasDocx" id="mode-hint" class="helper-text">
            DOCX 仅支持双语对照
          </p>

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
          <div v-if="s.sourceMode === 'mounted' && s.strategy === 'directory'">
            <span class="field-label">保存位置</span>
            <div class="path-group">
              <AppButton
                id="save-directory"
                class="path-picker"
                @click="pick('save')"
                ><span
                  >{{ s.save ? "保存到" : "任务内保存"
                  }}<strong>{{
                    s.save
                      ? pathLabel(s.save.storage, s.save.path)
                      : "完成后下载"
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
          <p
            v-if="
              s.sourceMode === 'mounted' && s.strategy === 'sibling_overwrite'
            "
            id="save-hint"
            class="job-warning"
          >
            将覆盖来源文件，提交前需确认
          </p>
          <details class="settings">
            <summary>
              更多设置
              <span v-if="s.save">{{
                pathLabel(s.save.storage, s.save.path)
              }}</span>
            </summary>
            <div v-if="s.sourceMode === 'upload'" class="save-setting">
              <span class="field-label">保存位置</span>
              <div class="path-group">
                <AppButton
                  id="save-directory"
                  class="path-picker"
                  @click="pick('save')"
                  ><span
                    ><strong>{{
                      s.save
                        ? pathLabel(s.save.storage, s.save.path)
                        : "完成后下载"
                    }}</strong></span
                  ><span aria-hidden="true">›</span></AppButton
                >
                <AppButton
                  v-if="s.save"
                  id="clear-save-directory"
                  aria-label="清除保存位置"
                  @click="s.save = null"
                  >×</AppButton
                >
              </div>
            </div>
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
        </section>
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
        <div class="submit-summary">
          <strong
            >{{
              s.sourceMode === "upload"
                ? `${s.files.length} 个文件`
                : `已选 ${s.sources.length} 项`
            }}
          </strong>
        </div>
        <AppButton
          id="submit-job"
          type="submit"
          variant="primary"
          :busy="locked"
          >{{
            s.stage === "previewing"
              ? "正在预览…"
              : s.stage === "submitting"
                ? "正在提交…"
                : "开始翻译"
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
  </section>
</template>
