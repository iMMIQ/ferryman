<script setup lang="ts">
import type { Job } from "../../lib/types";
import { phase, statusLabel } from "../../lib/format";
defineProps<{ job: Job }>();
</script>
<template>
  <span class="status-slot" role="status" aria-atomic="true"
    ><Transition name="status" mode="out-in"
      ><span :key="phase(job)" class="status-chip" :class="phase(job)">{{
        statusLabel(job)
      }}</span></Transition
    ></span
  >
</template>
