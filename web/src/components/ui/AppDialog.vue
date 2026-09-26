<script setup lang="ts">
import { watch } from "vue";
import {
  DialogRoot,
  DialogPortal,
  DialogOverlay,
  DialogContent,
  DialogTitle,
  DialogDescription,
  DialogClose,
} from "reka-ui";
import AppButton from "./AppButton.vue";
const props = defineProps<{
  open: boolean;
  id: string;
  title: string;
  description?: string;
  busy?: boolean;
}>();
const emit = defineEmits<{ "update:open": [value: boolean] }>();
let returnFocus: HTMLElement | null = null;
watch(
  () => props.open,
  (value) => {
    if (value) returnFocus = document.activeElement as HTMLElement;
  },
  { flush: "sync" },
);
function restore(event: Event) {
  event.preventDefault();
  if (returnFocus?.isConnected && returnFocus.getClientRects().length)
    returnFocus.focus();
  else document.querySelector<HTMLElement>("#refresh-jobs")?.focus();
}
</script>
<template>
  <DialogRoot
    :open="open"
    @update:open="
      (value) => {
        if (!busy) emit('update:open', value);
      }
    "
  >
    <DialogPortal
      ><DialogOverlay class="dialog-overlay" />
      <DialogContent
        :id="id"
        class="app-dialog"
        v-bind="description ? {} : { 'aria-describedby': undefined }"
        @close-auto-focus="restore"
        @escape-key-down="
          (event) => {
            if (busy) event.preventDefault();
          }
        "
        @interact-outside="(event) => event.preventDefault()"
      >
        <header class="dialog-header">
          <DialogTitle>{{ title }}</DialogTitle
          ><DialogClose as-child
            ><AppButton
              variant="ghost"
              :disabled="busy"
              :aria-label="`关闭${title}`"
              >×</AppButton
            ></DialogClose
          >
        </header>
        <div class="dialog-body">
          <DialogDescription v-if="description" class="helper-text">{{
            description
          }}</DialogDescription
          ><slot />
        </div>
        <footer v-if="$slots.footer" class="dialog-footer">
          <slot name="footer" />
        </footer>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>
