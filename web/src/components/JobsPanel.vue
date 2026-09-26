<script setup lang="ts">
import { computed, ref, onBeforeUpdate, onUpdated } from "vue";
import type { JobsStore } from "../composables/useJobs";
import type { Job } from "../lib/types";
import {
  phases,
  pathLabel,
  modelNames,
  time,
  statusLabel,
  terminal,
} from "../lib/format";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
import AppDialog from "./ui/AppDialog.vue";
import EmptyState from "./ui/EmptyState.vue";
import StatusBadge from "./ui/StatusBadge.vue";
import JobActions from "./JobActions.vue";
import JobProgress from "./JobProgress.vue";
const props = defineProps<{ store: JobsStore }>();
defineEmits<{ create: [] }>();
const s = props.store.state,
  detailId = ref<string | null>(null),
  deleting = ref(false);
const job = computed(() =>
  detailId.value ? s.byId[detailId.value] : undefined,
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
  <section class="jobs-section" aria-labelledby="jobs-title">
    <div class="section-heading">
      <div>
        <span class="eyebrow">任务队列</span>
        <h2 id="jobs-title">
          翻译记录
          <small id="jobs-count">{{ s.loaded ? `${s.total} 项` : "" }}</small>
        </h2>
      </div>
      <AppButton
        id="refresh-jobs"
        :busy="s.loading"
        aria-label="刷新任务"
        @click="store.refresh"
        >↻</AppButton
      >
    </div>
    <div
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
    <AppNotice id="jobs-error" :message="s.error"
      ><AppButton id="retry-jobs" @click="store.refresh"
        >重试</AppButton
      ></AppNotice
    >
    <p id="jobs-updated" class="helper-text">
      {{ s.updated ? `列表最近更新 ${s.updated}` : "正在加载任务…" }}
    </p>
    <div v-if="s.jobs.length" class="table-shell desktop-jobs">
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
          <tr v-for="item in s.jobs" :key="item.id" :data-id="item.id">
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
        v-for="item in s.jobs"
        :key="item.id"
        class="job-card"
        :data-id="item.id"
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
      v-if="!s.jobs.length && !s.error"
      id="empty-state"
      :title="s.loaded ? '暂无任务' : '正在加载任务…'"
      description="选择文档后，翻译进度会显示在这里"
      ><AppButton
        v-if="s.loaded"
        id="empty-create"
        variant="primary"
        @click="$emit('create')"
        >选择文档开始翻译</AppButton
      ></EmptyState
    >
    <nav
      v-if="s.cursors.length > 1 || s.next"
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
      <AppNotice id="job-detail-error" :message="job && s.errors[job.id]" />
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
