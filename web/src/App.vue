<script setup lang="ts">
import { ref, nextTick } from "vue";
import { useJobs } from "./composables/useJobs";
import { useRuntime } from "./composables/useRuntime";
import JobsPanel from "./components/JobsPanel.vue";
import SubmissionPanel from "./components/SubmissionPanel.vue";
import RuntimePanel from "./components/RuntimePanel.vue";
const jobs = useJobs(),
  runtime = useRuntime(),
  workspace = ref<"create" | "jobs">("create"),
  submission = ref<InstanceType<typeof SubmissionPanel>>();
async function completed() {
  await jobs.first();
  workspace.value = "jobs";
}
async function create() {
  workspace.value = "create";
  await nextTick();
  submission.value?.chooseFile();
}
</script>
<template>
  <div class="app" :data-workspace="workspace">
    <RuntimePanel :store="runtime" />
    <nav class="workspace-tabs" aria-label="工作区">
      <button
        data-workspace="create"
        :aria-pressed="workspace === 'create'"
        @click="workspace = 'create'"
      >
        新建翻译</button
      ><button
        data-workspace="jobs"
        :aria-pressed="workspace === 'jobs'"
        @click="workspace = 'jobs'"
      >
        任务 <span id="active-job-count">{{ jobs.state.active }}</span>
      </button>
    </nav>
    <main class="workspace">
      <SubmissionPanel
        ref="submission"
        :runtime="runtime"
        :completed="completed"
      /><JobsPanel :store="jobs" @create="create" />
    </main>
  </div>
</template>
