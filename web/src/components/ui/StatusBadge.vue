<script setup lang="ts">
import type { Job } from "../../lib/types";
import { phase, statusLabel } from "../../lib/format";
defineProps<{ job: Job }>();
</script>
<template>
  <span class="status-chip" :class="phase(job)">{{ statusLabel(job) }}</span>
</template>
