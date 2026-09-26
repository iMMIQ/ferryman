<script setup lang="ts">
import { computed, ref, onBeforeUpdate, onUpdated } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { api } from "../lib/api";
import type { JobsStore } from "../composables/useJobs";
import type { Job } from "../lib/types";
import {
  phases,
  pathLabel,
  modelNames,
  time,
  statusLabel,
  errorText,
  terminal,
} from "../lib/format";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
import AppDialog from "./ui/AppDialog.vue";
import EmptyState from "./ui/EmptyState.vue";
import StatusBadge from "./ui/StatusBadge.vue";
import JobActions from "./JobActions.vue";
import JobProgress from "./JobProgress.vue";
const props = defineProps<{
  store: JobsStore;
  compact?: boolean;
  createdIds?: string[];
}>();
const visibleJobs = computed(() =>
  props.compact ? props.store.state.recent : props.store.state.jobs,
);
const loaded = computed(() =>
  props.compact ? props.store.state.recentLoaded : props.store.state.loaded,
);
const error = computed(() =>
  props.compact ? props.store.state.recentError : props.store.state.error,
);
defineEmits<{ create: []; browse: [] }>();
const s = props.store.state,
  detailId = ref<string | null>(null),
  deleting = ref(false);
const detail = useQuery({
  queryKey: computed(() => ["jobs", "detail", detailId.value]),
  enabled: computed(() => !!detailId.value),
  queryFn: ({ signal, queryKey }) =>
    api<Job>(`/api/jobs/${queryKey[2]}`, { signal }),
  refetchInterval: (query) =>
    query.state.data && terminal(query.state.data) ? false : 2500,
});
const job = computed(() =>
  detailId.value ? (detail.data.value ?? s.byId[detailId.value]) : undefined,
);
let priorFocus: HTMLElement | null = null,
  focusedRow: Element | null = null;
onBeforeUpdate(() => {
  priorFocus = document.activeElement as HTMLElement;
  focusedRow = priorFocus?.closest("tr[data-id],.job-card");
});
onUpdated(() => {
  if (
    focusedRow &&
    priorFocus &&
    (!priorFocus.isConnected || !priorFocus.getClientRects().length)
  ) {
    const fallback = Array.from(
      document.querySelectorAll<HTMLElement>(".file-title,#refresh-jobs"),
    ).find((el) => el.getClientRects().length);
    fallback?.focus({ preventScroll: true });
  }
});
const fields = computed(() =>
  job.value
    ? {
        文件名: job.value.filename,
        状态: statusLabel(job.value),
        模型: modelNames[job.value.preset],
        目标语言: job.value.target,
        输出方式: job.value.mode === "replace" ? "仅译文" : "双语对照",
        来源:
          job.value.source_path != null
            ? pathLabel(
                job.value.source_storage || "documents",
                job.value.source_path,
              )
            : "上传文件",
        保存位置:
          job.value.save_path != null
            ? pathLabel(
                job.value.save_storage || "documents",
                job.value.save_path,
              )
            : "任务内保存 · 完成后下载",
        创建时间: time(job.value.created_at),
        进度: `已译 ${job.value.translated}/${job.value.total} · 已处理 ${job.value.completed} · 未译 ${job.value.failed_segments}`,
        诊断: job.value.error || "—",
      }
    : {},
);
function details(item: Job) {
  detailId.value = item.id;
  deleting.value = false;
}
async function action(item: Job, action: "retry" | "cancel" | "delete") {
  if (action === "delete") {
    deleting.value = true;
    return;
  }
  await props.store.act(item.id, action);
}
async function remove() {
  if (job.value && (await props.store.act(job.value.id, "delete")))
    detailId.value = null;
}
</script>
<template>
  <section
    class="jobs-section"
    :class="{ compact }"
    aria-labelledby="jobs-title"
  >
    <div class="section-heading">
      <div>
        <h2 id="jobs-title">
          {{ compact ? "最近任务" : "翻译记录" }}
          <small v-if="!compact" id="jobs-count">{{
            s.loaded ? `${s.total} 项` : ""
          }}</small>
        </h2>
      </div>
      <AppButton v-if="compact" variant="ghost" @click="$emit('browse')"
        >查看全部</AppButton
      >
      <AppButton
        v-else
        id="refresh-jobs"
        :busy="s.loading"
        aria-label="刷新任务"
        @click="store.refresh"
        >↻</AppButton
      >
    </div>
    <div
      v-if="!compact"
      id="job-status-filter"
      class="filter-bar"
      role="group"
      aria-label="筛选任务状态"
    >
      <button
        v-for="p in phases"
        :key="p.value"
        :data-job-phase="p.value"
        :aria-pressed="s.phase === p.value"
        @click="store.filter(p.value)"
      >
        {{ p.label }}
      </button>
    </div>
    <AppNotice id="jobs-error" :message="error"
      ><AppButton id="retry-jobs" @click="store.refresh"
        >重试</AppButton
      ></AppNotice
    >
    <p v-if="!compact" id="jobs-updated" class="helper-text">
      {{ s.updated ? `列表最近更新 ${s.updated}` : "正在加载任务…" }}
    </p>
    <div v-if="visibleJobs.length && !compact" class="table-shell desktop-jobs">
      <table>
        <thead>
          <tr>
            <th>文件</th>
            <th>模型</th>
            <th>状态</th>
            <th>进度</th>
            <th>操作</th>
          </tr>
        </thead>
        <tbody id="jobs-body">
          <tr
            v-for="item in visibleJobs"
            :key="item.id"
            :data-id="item.id"
            :class="{ 'new-job': createdIds?.includes(item.id) }"
          >
            <td>
              <button
                class="file-title"
                :title="item.filename"
                @click="details(item)"
              >
                {{ item.filename }}</button
              ><span class="file-meta"
                >{{ item.target }} ·
                {{ item.mode === "replace" ? "仅译文" : "双语对照" }}</span
              >
              <p v-if="item.failed_segments" class="job-warning">
                {{ item.failed_segments }} 段未翻译
              </p>
              <AppNotice :message="s.errors[item.id]" />
            </td>
            <td>
              <span class="model-chip">{{
                item.preset === "7b-fp8" ? "7B" : "30B"
              }}</span>
            </td>
            <td><StatusBadge :job="item" /></td>
            <td><JobProgress :job="item" /></td>
            <td>
              <JobActions
                :job="item"
                :pending="s.pending[item.id]"
                @action="action(item, $event)"
                @details="details(item)"
              />
            </td>
          </tr>
        </tbody>
      </table>
    </div>
    <div class="mobile-jobs">
      <article
        v-for="item in visibleJobs"
        :key="item.id"
        class="job-card"
        :data-id="item.id"
        :class="{ 'new-job': createdIds?.includes(item.id) }"
      >
        <button class="file-title" @click="details(item)">
          {{ item.filename }}
        </button>
        <div class="card-meta">
          <span
            >{{ item.target }} ·
            {{ item.preset === "7b-fp8" ? "7B" : "30B" }}</span
          ><StatusBadge :job="item" />
        </div>
        <p v-if="item.error" class="job-error-summary">{{ item.error }}</p>
        <p v-if="item.failed_segments" class="job-warning">
          {{ item.failed_segments }} 段未翻译{{
            item.result_available
              ? "，可下载部分结果或补译。"
              : "，请查看详情后重试。"
          }}
        </p>
        <JobProgress :job="item" /><AppNotice
          :message="s.errors[item.id]"
        /><JobActions
          :job="item"
          :pending="s.pending[item.id]"
          @action="action(item, $event)"
          @details="details(item)"
        />
      </article>
    </div>
    <EmptyState
      v-if="!visibleJobs.length && !error"
      id="empty-state"
      :title="loaded ? '暂无任务' : '正在加载任务…'"
      description="选择文档后，翻译进度会显示在这里"
      ><AppButton
        v-if="loaded"
        id="empty-create"
        variant="primary"
        @click="$emit('create')"
        >选择文档开始翻译</AppButton
      ></EmptyState
    >
    <nav
      v-if="!compact && (s.cursors.length > 1 || s.next)"
      class="jobs-pagination"
      aria-label="翻译记录分页"
    >
      <AppButton
        id="jobs-previous"
        :disabled="s.cursors.length === 1 || s.loading"
        @click="store.previousPage"
        >上一页</AppButton
      ><span>第 {{ s.cursors.length }} 页</span
      ><AppButton
        id="jobs-next"
        :disabled="!s.next || s.loading"
        @click="store.next"
        >下一页</AppButton
      >
    </nav>
    <p class="background-note">任务在后台继续，离开页面也不会中断。</p>
    <AppDialog
      id="job-detail-dialog"
      :open="!!job"
      title="任务详情"
      @update:open="detailId = null"
    >
      <dl id="job-detail-content" class="detail-fields">
        <template v-for="(value, key) in fields" :key="key"
          ><dt>{{ key }}</dt>
          <dd>{{ value }}</dd></template
        >
      </dl>
      <p class="helper-text">
        重试会优先复用已完成的翻译；旧任务若缺少翻译记录，可能需要重新翻译。
      </p>
      <AppNotice
        id="job-detail-error"
        :message="
          (job && s.errors[job.id]) ||
          (detail.error.value ? errorText(detail.error.value) : '')
        "
      />
      <div v-if="deleting && job && terminal(job)" class="notice error">
        <span>删除这条记录及任务内文件？保存到文稿或网盘的结果不会删除。</span
        ><AppButton variant="danger" :busy="s.pending[job.id]" @click="remove"
          >确认删除</AppButton
        ><AppButton @click="deleting = false">保留记录</AppButton>
      </div>
      <template #footer
        ><JobActions
          v-if="job"
          id="job-detail-actions"
          :job="job"
          detail
          :pending="s.pending[job.id]"
          @action="action(job, $event)"
      /></template>
    </AppDialog>
  </section>
</template>
