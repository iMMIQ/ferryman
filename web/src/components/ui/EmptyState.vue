<script setup lang="ts">
defineProps<{ title: string; description?: string }>();
</script>
<template>
  <div class="empty-state">
    <strong>{{ title }}</strong>
    <p v-if="description">{{ description }}</p>
    <slot />
  </div>
</template>
