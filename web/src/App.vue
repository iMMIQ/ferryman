<script setup lang="ts">
import { ref, nextTick, computed } from "vue";
import { useJobs } from "./composables/useJobs";
import { useModels } from "./composables/useModels";
import JobsPanel from "./components/JobsPanel.vue";
import SubmissionPanel from "./components/SubmissionPanel.vue";
import ModelsPanel from "./components/ModelsPanel.vue";
import AppIcon from "./components/ui/AppIcon.vue";
const jobs = useJobs(),
  models = useModels(),
  workspace = ref<"create" | "jobs">("create");
const submission = ref<InstanceType<typeof SubmissionPanel>>();
const modelPanel = ref<InstanceType<typeof ModelsPanel>>();
const heading = ref<HTMLElement>();
const submitted = ref("");
const createdIds = ref<string[]>([]);
const connection = computed(() =>
  !models.loaded.value
    ? "连接中"
    : !models.connected.value
      ? "连接中断"
      : `${models.models.value.filter((m) => m.available).length} 个可选模型`,
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
function activateView(element: Element) {
  (element as HTMLElement).inert = false;
}
function deactivateView(element: Element) {
  (element as HTMLElement).inert = true;
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
          id="models-open"
          data-workspace="models"
          aria-haspopup="dialog"
          @click="modelPanel?.manage()"
        >
          <AppIcon name="model" /><span>模型与 API</span>
        </button>
      </nav>
      <div class="sidebar-connection">
        <span
          class="status-dot"
          :class="models.connected.value ? 'ready' : 'neutral'"
        ></span
        ><span id="runtime-label">{{ connection }}</span>
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
        </div>
      </header>
      <main class="workspace">
        <Transition
          name="workspace"
          @before-enter="activateView"
          @before-leave="deactivateView"
          @leave-cancelled="activateView"
        >
          <SubmissionPanel
            v-show="workspace === 'create'"
            ref="submission"
            :models="models"
            :completed="completed"
            @manage-models="modelPanel?.manage()"
          />
        </Transition>
        <Transition
          name="workspace"
          @before-enter="activateView"
          @before-leave="deactivateView"
          @leave-cancelled="activateView"
        >
          <div v-show="workspace === 'jobs'" class="jobs-workspace">
            <p
              v-if="submitted && workspace === 'jobs'"
              class="submission-success"
              role="status"
            >
              {{ submitted }}
            </p>
            <JobsPanel
              :store="jobs"
              :created-ids="createdIds"
              @create="create"
              @browse="browse"
            />
          </div>
        </Transition>
      </main>
    </div>
    <ModelsPanel ref="modelPanel" :store="models" />
  </div>
</template>
