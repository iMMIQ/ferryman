<script setup lang="ts">
import type { Job } from "../lib/types";
import { phase } from "../lib/format";
defineProps<{ job: Job }>();
</script>
<template>
  <div class="progress-wrap">
    <progress
      :class="phase(job)"
      :value="job.translated"
      :max="job.total || 1"
      :aria-label="`${job.filename} 翻译进度`"
    /><span class="progress-label">{{
      job.total > 0
        ? `已译 ${job.translated}/${job.total}`
        : job.status === "queued"
          ? "等待调度"
          : "准备中"
    }}</span>
  </div>
</template>
