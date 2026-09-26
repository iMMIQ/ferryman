<script setup lang="ts">
import { ref, nextTick, computed } from "vue";
import { useJobs } from "./composables/useJobs";
import { useRuntime } from "./composables/useRuntime";
import { runtimeNames } from "./lib/format";
import JobsPanel from "./components/JobsPanel.vue";
import SubmissionPanel from "./components/SubmissionPanel.vue";
import RuntimePanel from "./components/RuntimePanel.vue";
import AppIcon from "./components/ui/AppIcon.vue";
const jobs = useJobs(),
  runtime = useRuntime(),
  workspace = ref<"create" | "jobs">("create");
const submission = ref<InstanceType<typeof SubmissionPanel>>();
const runtimePanel = ref<InstanceType<typeof RuntimePanel>>();
const heading = ref<HTMLElement>();
const submitted = ref("");
const createdIds = ref<string[]>([]);
const connection = computed(() =>
  !runtime.state.loaded
    ? "连接中"
    : !runtime.state.connected
      ? "连接中断"
      : `已连接 · ${runtimeNames[runtime.state.runtime.state] || "待机"}`,
);
async function navigate(view: "create" | "jobs") {
  workspace.value = view;
  await nextTick();
  heading.value?.focus({ preventScroll: true });
  window.scrollTo({ top: 0, behavior: "instant" });
}
async function completed(ids: string[], message: string) {
  await jobs.first();
  submitted.value = message;
  createdIds.value = ids;
  await navigate("jobs");
}
async function create() {
  await navigate("create");
  submission.value?.chooseFile();
}
async function browse() {
  await jobs.first();
  await navigate("jobs");
}
</script>
<template>
  <div class="app" :data-workspace="workspace">
    <a class="skip-link" href="#workspace-title">跳至工作区</a>
    <aside class="app-sidebar">
      <div class="brand-block">
        <span class="brand-mark" aria-hidden="true">F</span>
        <div>
          <strong>Ferryman</strong>
          <p>文档翻译</p>
        </div>
      </div>
      <nav class="workspace-tabs" aria-label="工作区">
        <button
          data-workspace="create"
          :aria-pressed="workspace === 'create'"
          @click="navigate('create')"
        >
          <AppIcon name="plus" /><span>新建翻译</span>
        </button>
        <button
          data-workspace="jobs"
          :aria-pressed="workspace === 'jobs'"
          @click="navigate('jobs')"
        >
          <AppIcon name="tasks" /><span>翻译任务</span
          ><span
            v-if="jobs.state.active"
            id="active-job-count"
            class="nav-count"
            :aria-label="`${jobs.state.active} 个进行中任务`"
            >{{ jobs.state.active }}</span
          >
        </button>
        <button
          data-workspace="models"
          aria-haspopup="dialog"
          @click="runtimePanel?.manage()"
        >
          <AppIcon name="model" /><span>模型与算力</span>
        </button>
      </nav>
      <div class="sidebar-connection">
        <span
          class="status-dot"
          :class="runtime.state.connected ? 'ready' : 'neutral'"
        ></span
        ><span>{{ connection }}</span>
      </div>
    </aside>
    <div class="app-main">
      <div class="mobile-brand">
        <span class="brand-mark" aria-hidden="true">F</span
        ><strong>Ferryman</strong
        ><span class="connection-chip">{{ connection }}</span>
      </div>
      <header class="page-heading">
        <div>
          <h1 id="workspace-title" ref="heading" tabindex="-1">
            {{ workspace === "create" ? "新建翻译" : "翻译任务" }}
          </h1>
          <p>
            {{
              workspace === "create"
                ? "文档、电子书与字幕，一处翻译。"
                : `${jobs.state.active} 个进行中 · 所有翻译记录，一目了然。`
            }}
          </p>
        </div>
        <RuntimePanel ref="runtimePanel" :store="runtime" />
      </header>
      <main class="workspace">
        <SubmissionPanel
          v-show="workspace === 'create'"
          ref="submission"
          :runtime="runtime"
          :completed="completed"
        />
        <div class="jobs-workspace">
          <p
            v-if="submitted && workspace === 'jobs'"
            class="submission-success"
            role="status"
          >
            {{ submitted }} 进度会自动更新。
          </p>
          <JobsPanel
            :store="jobs"
            :created-ids="createdIds"
            :compact="workspace === 'create'"
            @create="create"
            @browse="browse"
          />
        </div>
      </main>
    </div>
  </div>
</template>
