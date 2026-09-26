<script setup lang="ts">
import { ref, computed } from "vue";
import type { RuntimeStore } from "../composables/useRuntime";
import type { Model } from "../lib/types";
import { json } from "../lib/api";
import { bytes, modelNames, modelStates, runtimeNames } from "../lib/format";
import AppButton from "./ui/AppButton.vue";
import AppDialog from "./ui/AppDialog.vue";
import AppNotice from "./ui/AppNotice.vue";
import SegmentedControl from "./ui/SegmentedControl.vue";
const props = defineProps<{ store: RuntimeStore }>(),
  s = props.store.state,
  open = ref(false),
  confirmation = ref<{ key: string; path: string; message: string } | null>(
    null,
  );
const selected = computed(() =>
  s.catalog.models.find((m) => m.preset === s.preset),
);
const preparing = (model?: Model) =>
  !!model && ["benchmarking", "downloading", "verifying"].includes(model.state);
const running = computed(
  () => s.runtime.state === "ready" && s.runtime.preset === s.preset,
);
const startLabel = computed(() =>
  !s.connected || !s.catalogConnected
    ? "等待连接"
    : running.value
      ? "运行中"
      : preparing(selected.value)
        ? "准备中"
        : selected.value?.state === "ready"
          ? "启动模型"
          : selected.value?.state === "paused"
            ? "继续下载"
            : "下载模型",
);
const startDisabled = computed(
  () =>
    !s.connected ||
    !s.catalogConnected ||
    running.value ||
    preparing(selected.value) ||
    ["starting", "stopping"].includes(s.runtime.state),
);
const connectionError = computed(() =>
  !s.loaded
    ? ""
    : !s.connected
      ? "无法连接算力舱。任务记录独立保留，恢复连接后可继续。"
      : !s.catalogConnected
        ? "模型目录读取失败，正在重试。运行状态仍可查看。"
        : "",
);
const stages: Record<string, string> = {
  starting_process: "正在创建推理进程",
  loading_weights: "正在加载模型权重",
  compiling_kernels: "正在编译推理内核",
  capturing_graphs: "正在构建 CUDA Graph",
  starting_server: "正在启动推理服务",
  ready: "模型已就绪",
  failed: "启动失败",
};
async function manage() {
  open.value = true;
  confirmation.value = null;
  s.error = "";
  await Promise.all([props.store.refresh(), props.store.storage()]);
}
async function start() {
  await props.store.mutate(
    "runtime",
    selected.value?.state === "ready"
      ? "/api/runtime/start"
      : `/api/models/${s.preset}/download`,
    json(
      selected.value?.state === "ready"
        ? { preset: s.preset }
        : { source: s.source },
    ),
  );
}
async function modelAction(model: Model) {
  if (model.state === "ready") {
    confirmation.value = {
      key: model.preset,
      path: `/api/models/${model.preset}`,
      message: `删除 ${modelNames[model.preset]}？以后使用时需要重新下载。`,
    };
    return;
  }
  await props.store.mutate(
    model.preset,
    `/api/models/${model.preset}/${preparing(model) ? "pause" : "download"}`,
    json({ source: s.source }),
  );
}
async function confirm() {
  const action = confirmation.value;
  if (!action) return;
  if (await props.store.mutate(action.key, action.path, { method: "DELETE" }))
    confirmation.value = null;
}
defineExpose({ manage });
</script>
<template>
  <div class="runtime-overview">
    <div class="runtime-summary" aria-label="模型运行状态">
      <span
        class="status-dot"
        :class="s.connected ? s.runtime.state : 'neutral'"
      ></span
      ><span>{{
        s.runtime.preset ? modelNames[s.runtime.preset] : "算力舱"
      }}</span
      ><strong id="runtime-label">{{
        !s.loaded
          ? "连接中"
          : s.connected
            ? runtimeNames[s.runtime.state] || s.runtime.state
            : "连接中断"
      }}</strong>
    </div>
    <AppButton id="manage-models" variant="ghost" @click="manage"
      >管理模型</AppButton
    >
  </div>
  <div class="runtime-feedback">
    <AppNotice id="connection-error" :message="connectionError"
      ><AppButton id="retry-connection" @click="store.refresh"
        >重新连接</AppButton
      ></AppNotice
    >
    <section
      v-if="s.connected && ['starting', 'failed'].includes(s.runtime.state)"
      id="runtime-startup"
      class="runtime-startup"
    >
      <strong>{{
        stages[s.runtime.startup_stage || s.runtime.state] || "模型启动"
      }}</strong
      ><progress
        :value="s.runtime.startup_progress || 0"
        max="100"
        aria-label="模型启动进度"
      />
      <p class="helper-text">
        已用 {{ Math.round(s.runtime.startup_elapsed_seconds || 0) }} 秒 ·
        {{
          s.runtime.estimated_remaining_seconds != null
            ? `预计还需 ${Math.round(s.runtime.estimated_remaining_seconds)} 秒`
            : "完成后自动继续翻译"
        }}
      </p>
      <details>
        <summary>
          启动日志 · {{ s.runtime.recent_logs?.length || 0 }} 行
        </summary>
        <pre>{{
          s.runtime.recent_logs?.join("\n") ||
          s.runtime.last_error ||
          "等待 vLLM 输出…"
        }}</pre>
      </details>
    </section>
  </div>
  <AppDialog
    id="model-dialog"
    v-model:open="open"
    title="模型管理"
    description="翻译任务会自动准备模型。仅在需要预热或释放资源时手动操作。"
  >
    <AppNotice id="model-error" :message="s.error" />
    <section class="model-section">
      <h3>手动管理运行模型</h3>
      <SegmentedControl
        v-model="s.preset"
        name="runtime-preset"
        label="待启动模型"
        :options="[
          { value: '7b-fp8', label: '7B FP8' },
          { value: '30b-fp8', label: '30B FP8' },
        ]"
      />
      <div class="runtime-actions">
        <AppButton
          id="start-runtime"
          :disabled="startDisabled"
          :busy="s.pending.runtime"
          @click="start"
          >{{ startLabel }}</AppButton
        ><AppButton
          id="stop-runtime"
          :disabled="
            !s.connected ||
            ['stopped', 'stopping', 'unknown'].includes(s.runtime.state)
          "
          :busy="s.pending.runtime"
          @click="
            store.mutate('runtime', '/api/runtime/stop', { method: 'POST' })
          "
          >卸载模型</AppButton
        >
      </div>
    </section>
    <section class="model-section">
      <h3>翻译模型</h3>
      <p class="helper-text">
        {{
          s.storage
            ? `模型目录 ${bytes(s.storage.model_bytes)} · 未完成 ${bytes(s.storage.partial_bytes)} · `
            : ""
        }}可用
        {{ bytes(s.storage?.available_bytes ?? s.catalog.available_bytes) }}
      </p>
      <div id="model-list">
        <div
          v-for="model in s.catalog.models"
          :key="model.preset"
          class="model-row"
        >
          <div class="model-row-main">
            <strong>{{ modelNames[model.preset] }}</strong
            ><span class="model-state">{{
              modelStates[model.state] || model.state
            }}</span>
            <p class="helper-text">
              {{
                model.last_error ||
                (model.state === "ready"
                  ? bytes(model.downloaded_bytes)
                  : `${bytes(model.downloaded_bytes)} / ${bytes(model.expected_bytes)}`)
              }}
            </p>
            <progress
              v-if="preparing(model) || model.state === 'paused'"
              :value="model.downloaded_bytes"
              :max="model.expected_bytes || 1"
              :aria-label="`${modelNames[model.preset]} 下载进度`"
            />
          </div>
          <AppButton
            :variant="model.state === 'ready' ? 'danger' : 'secondary'"
            :busy="s.pending[model.preset]"
            :disabled="!s.catalogConnected"
            @click="modelAction(model)"
            >{{
              preparing(model)
                ? "暂停"
                : model.state === "ready"
                  ? "删除"
                  : model.state === "paused"
                    ? "继续"
                    : "下载"
            }}</AppButton
          >
        </div>
      </div>
    </section>
    <section class="model-section">
      <div class="section-heading">
        <div>
          <h3>下载来源</h3>
          <p class="helper-text">测速约使用 24 MiB 流量</p>
        </div>
        <AppButton
          id="benchmark-sources"
          :busy="s.pending.benchmark || s.catalog.benchmark.state === 'running'"
          @click="
            store.mutate(
              'benchmark',
              '/api/model-sources/benchmark',
              json({ preset: s.preset }),
            )
          "
          >{{
            s.catalog.benchmark.state === "running" ? "测速中" : "重新测速"
          }}</AppButton
        >
      </div>
      <label for="model-source" class="field-label">默认源</label
      ><select id="model-source" v-model="s.source">
        <option value="auto">自动选择（推荐）</option>
        <option value="modelscope">ModelScope</option>
        <option value="hf_mirror">HF Mirror</option>
        <option value="huggingface">Hugging Face</option>
      </select>
      <div class="benchmark-results">
        <p v-if="!s.catalog.benchmark.results.length" class="helper-text">
          首次下载时会自动测速
        </p>
        <p v-for="result in s.catalog.benchmark.results" :key="result.source">
          {{ result.label }} ·
          {{
            result.available
              ? `${bytes(result.bytes_per_second)}/s · ${result.latency_ms} ms`
              : "无法连接"
          }}
          {{
            result.source === s.catalog.benchmark.recommended ? " · 推荐" : ""
          }}
        </p>
      </div>
    </section>
    <section class="model-section cache-section">
      <div>
        <h3>推理缓存</h3>
        <span class="helper-text"
          >{{ s.storage ? bytes(s.storage.cache_bytes) : "正在统计" }} ·
          可安全清理</span
        >
      </div>
      <AppButton
        :busy="s.pending.cache"
        @click="
          confirmation = {
            key: 'cache',
            path: '/api/runtime-cache',
            message: '清理推理缓存？模型不会删除，但下次启动需要重新编译。',
          }
        "
        >清理缓存</AppButton
      >
    </section>
    <div v-if="confirmation" class="notice error">
      <span>{{ confirmation.message }}</span
      ><AppButton
        variant="danger"
        :busy="s.pending[confirmation.key]"
        @click="confirm"
        >确认</AppButton
      ><AppButton
        :disabled="s.pending[confirmation.key]"
        @click="confirmation = null"
        >取消</AppButton
      >
    </div>
    <template #footer
      ><span class="helper-text"
        >模型保存在当前算力舱，切换算力舱后需要重新下载。</span
      ><AppButton id="done-model-dialog" variant="primary" @click="open = false"
        >完成</AppButton
      ></template
    >
  </AppDialog>
</template>
