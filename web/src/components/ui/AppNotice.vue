<script setup lang="ts">
defineProps<{ message?: string; tone?: "error" | "info" }>();
</script>
<template>
  <div
    v-if="message"
    class="notice"
    :class="tone || 'error'"
    :role="tone === 'info' ? 'status' : 'alert'"
  >
    <span>{{ message }}</span
    ><slot />
  </div>
</template>
