<script setup lang="ts">
withDefaults(defineProps<{ name: string; size?: number }>(), { size: 20 });
const paths: Record<string, string> = {
  plus: "M12 5v14M5 12h14",
  tasks: "M9 6h11M9 12h11M9 18h11M4 6h.01M4 12h.01M4 18h.01",
  model: "m12 3 8 4.5v9L12 21l-8-4.5v-9L12 3Zm0 9 8-4.5M12 12 4 7.5M12 12v9",
  upload: "M12 16V3m-5 5 5-5 5 5M4 15v5a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-5",
  file: "M14 2H5v20h14V7l-5-5Zm0 0v6h5M8 13h8M8 17h6",
  folder: "M3 6h6l2 2h10v12H3V6Z",
  arrow: "M4 12h16m-6-6 6 6-6 6",
  close: "m6 6 12 12M6 18 18 6",
  check: "m5 12 4 4L19 6",
};
</script>
<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="1.7"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
    focusable="false"
  >
    <path :d="paths[name] || paths.file" />
  </svg>
</template>
