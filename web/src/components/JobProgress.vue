<script setup lang="ts">
import type { Job } from "../lib/types";
import { phase } from "../lib/format";
import { computed } from "vue";
const props = defineProps<{ job: Job }>();
const fraction = computed(() =>
  Math.max(0, Math.min(1, props.job.translated / (props.job.total || 1))),
);
</script>
<template>
  <div class="progress-wrap">
    <div
      class="job-progress-track"
      role="progressbar"
      :class="phase(job)"
      :aria-valuenow="job.total > 0 ? job.translated : undefined"
      aria-valuemin="0"
      :aria-valuemax="job.total || 1"
      :aria-label="`${job.filename} 翻译进度`"
    >
      <span
        class="job-progress-fill"
        :style="{ transform: `scaleX(${fraction})` }"
      ></span>
    </div>
    <span class="progress-label">{{
      job.total > 0
        ? `已译 ${job.translated}/${job.total} 段 · ${Math.min(100, Math.round((job.translated / job.total) * 100))}%`
        : job.status === "queued"
          ? "等待调度"
          : "准备中"
    }}</span>
  </div>
</template>
