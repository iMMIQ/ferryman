<script setup lang="ts">
import type { Job } from "../lib/types";
import { phase, retryable, terminal } from "../lib/format";
import AppButton from "./ui/AppButton.vue";
defineProps<{ job: Job; pending?: boolean; detail?: boolean }>();
defineEmits<{ action: [action: "retry" | "cancel" | "delete"]; details: [] }>();
</script>
<template>
  <div class="row-actions">
    <a
      v-if="job.status === 'completed' || job.result_available"
      class="button ghost download-action"
      :href="`/api/jobs/${job.id}/result`"
      >{{ phase(job) === "partial" ? "下载部分" : "下载结果" }}</a
    >
    <AppButton
      v-if="retryable(job)"
      variant="ghost"
      data-action="retry"
      :busy="pending"
      @click="$emit('action', 'retry')"
      >{{
        job.status === "completed"
          ? "补译"
          : job.status === "cancelled"
            ? "继续"
            : "重试"
      }}</AppButton
    >
    <AppButton
      v-if="!terminal(job)"
      variant="ghost"
      data-action="cancel"
      :busy="pending"
      @click="$emit('action', 'cancel')"
      >取消</AppButton
    >
    <AppButton
      v-if="detail && terminal(job)"
      variant="danger"
      data-action="delete"
      :busy="pending"
      @click="$emit('action', 'delete')"
      >删除记录</AppButton
    >
    <AppButton
      v-if="!detail"
      variant="ghost"
      data-action="details"
      @click="$emit('details')"
      >更多</AppButton
    >
  </div>
</template>
