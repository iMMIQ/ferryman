<script setup lang="ts" generic="T extends string">
defineProps<{
  modelValue: T;
  name: string;
  label: string;
  options: readonly { value: T; label: string; disabled?: boolean }[];
  disabled?: boolean;
}>();
defineEmits<{ "update:modelValue": [value: T] }>();
</script>
<template>
  <fieldset class="segmented-field">
    <legend>{{ label }}</legend>
    <div class="segmented">
      <label
        v-for="option in options"
        :key="option.value"
        class="segment"
        :class="{ active: modelValue === option.value }"
        ><input
          type="radio"
          :name="name"
          :value="option.value"
          :checked="modelValue === option.value"
          :disabled="disabled || option.disabled"
          @change="$emit('update:modelValue', option.value)"
        /><span>{{ option.label }}</span></label
      >
    </div>
  </fieldset>
</template>
