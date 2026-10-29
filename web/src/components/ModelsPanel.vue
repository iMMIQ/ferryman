<script setup lang="ts">
import { reactive, ref } from "vue";
import type { ModelsStore } from "../composables/useModels";
import type { ModelInfo } from "../lib/generated/ModelInfo";
import type { PromptAdapter } from "../lib/generated/PromptAdapter";
import { errorText } from "../lib/format";
import AppDialog from "./ui/AppDialog.vue";
import AppButton from "./ui/AppButton.vue";
import AppNotice from "./ui/AppNotice.vue";
const props = defineProps<{ store: ModelsStore }>();
const open = ref(false),
  editing = ref(false),
  busy = ref(false),
  error = ref("");
const aliases = ref<string[]>([]);
const draft = reactive({
  id: null as string | null,
  name: "",
  base_url: "",
  model_id: "",
  api_key: "",
  adapter: "generic" as PromptAdapter,
  context_length: 2048,
  max_output_tokens: 768,
  concurrency: 4,
});
function manage() {
  open.value = true;
}
function edit(model?: ModelInfo) {
  Object.assign(
    draft,
    model
      ? { ...model.target, api_key: "" }
      : {
          id: null,
          name: "",
          base_url: "",
          model_id: "",
          api_key: "",
          adapter: "generic",
          context_length: 2048,
          max_output_tokens: 768,
          concurrency: 4,
        },
  );
  aliases.value = [];
  error.value = "";
  editing.value = true;
}
async function readModels() {
  busy.value = true;
  error.value = "";
  try {
    const result = await props.store.probe({
      base_url: draft.base_url,
      api_key: draft.api_key || null,
      id: draft.id,
    });
    aliases.value = result.models;
    if (!draft.model_id && aliases.value.length)
      draft.model_id = aliases.value[0]!;
    if (result.context_length) {
      draft.context_length = result.context_length;
      draft.max_output_tokens = Math.min(
        8192,
        Math.floor(result.context_length / 2),
      );
    }
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
  }
}
async function save() {
  busy.value = true;
  error.value = "";
  try {
    await props.store.save({ ...draft, api_key: draft.api_key || null });
    editing.value = false;
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
  }
}
async function remove(id: string) {
  busy.value = true;
  error.value = "";
  try {
    await props.store.remove(id);
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
  }
}
defineExpose({ manage });
</script>
<template>
  <AppButton id="models-open" @click="manage">模型与 API</AppButton>
  <AppDialog
    v-model:open="open"
    id="model-dialog"
    title="模型与 API"
    :busy="busy"
  >
    <p class="helper-text">
      {{
        store.discoveryEnabled.value
          ? "由微服发现已部署模型，或连接兼容 OpenAI Chat 的 API。"
          : "连接兼容 OpenAI Chat 的 API，配置用于翻译的模型。"
      }}
    </p>
    <div class="model-toolbar">
      <AppButton
        v-if="store.discoveryEnabled.value"
        id="discover-models"
        :disabled="store.state.pending || busy"
        @click="store.discover"
      >
        {{ store.state.pending ? "正在发现…" : "刷新自动发现" }}
      </AppButton>
      <AppButton id="add-custom-model" :disabled="busy" @click="edit()"
        >添加自定义 API</AppButton
      >
    </div>
    <AppNotice :message="store.state.error || store.error.value" />
    <p v-if="store.state.partial" class="helper-text" role="status">
      发现结果不完整，已保留之前的模型记录。
    </p>
    <ul v-if="store.state.warnings.length" class="helper-text">
      <li v-for="warning in store.state.warnings" :key="warning">
        {{ warning }}
      </li>
    </ul>
    <ul class="api-model-list" aria-label="可选模型">
      <li v-for="model in store.models.value" :key="model.target.id">
        <div>
          <strong>{{ model.target.name }}</strong>
          <p class="helper-text">
            {{ model.source === "custom" ? "自定义 API" : "自动发现" }} ·
            {{ model.available ? "可选择" : "当前不可用" }}
            <template v-if="model.devices.length">
              · {{ model.devices.join("、") }}</template
            >
          </p>
          <p class="helper-text api-address">
            {{ model.target.model_id }} · {{ model.target.base_url }}
          </p>
        </div>
        <div v-if="model.source === 'custom'" class="model-toolbar">
          <AppButton
            :disabled="busy"
            :aria-label="`编辑 ${model.target.name}`"
            @click="edit(model)"
            >编辑</AppButton
          >
          <AppButton
            :disabled="busy"
            :aria-label="`移除 ${model.target.name}`"
            @click="remove(model.target.id)"
            >移除</AppButton
          >
        </div>
      </li>
    </ul>
    <p v-if="!store.models.value.length" class="helper-text">
      {{
        store.discoveryEnabled.value
          ? "暂无模型。可以刷新自动发现，或添加自定义 API。"
          : "暂无模型，请添加自定义 API。"
      }}
    </p>
    <form
      v-if="editing"
      id="custom-api-form"
      class="api-model-form"
      @submit.prevent="save"
    >
      <fieldset :disabled="busy">
        <legend>{{ draft.id ? "编辑 API" : "添加自定义 API" }}</legend>
        <label
          >名称<input
            v-model="draft.name"
            name="api_name"
            required
            maxlength="512"
            placeholder="我的翻译模型"
        /></label>
        <label
          >API 地址<input
            v-model="draft.base_url"
            name="api_base_url"
            type="url"
            required
            placeholder="https://example.com/v1"
        /></label>
        <label
          >API Key<input
            v-model="draft.api_key"
            name="api_key"
            type="password"
            autocomplete="new-password"
            :placeholder="draft.id ? '留空保留已保存的 Key' : '本地服务可留空'"
        /></label>
        <AppButton id="read-api-models" @click="readModels"
          >读取模型列表</AppButton
        >
        <label
          >模型 ID<input
            v-model="draft.model_id"
            name="api_model_id"
            list="api-model-aliases"
            required
            maxlength="512"
        /></label>
        <datalist id="api-model-aliases">
          <option v-for="alias in aliases" :key="alias" :value="alias" />
        </datalist>
        <label
          >翻译提示词<select v-model="draft.adapter" name="api_adapter">
            <option value="generic">通用聊天模型</option>
            <option value="index_translate">Index-Translate</option>
          </select></label
        >
        <div class="tuning-grid">
          <label
            >上下文上限<input
              v-model.number="draft.context_length"
              name="api_context"
              type="number"
              min="512"
              max="2000000"
              required
          /></label>
          <label
            >译文输出上限<input
              v-model.number="draft.max_output_tokens"
              name="api_output"
              type="number"
              min="1"
              :max="Math.floor(draft.context_length / 2)"
              required
          /></label>
          <label
            >每任务并发数<input
              v-model.number="draft.concurrency"
              name="api_concurrency"
              type="number"
              min="1"
              max="32"
              required
          /></label>
        </div>
        <p class="helper-text">
          API Key 仅保存在应用后端。上下文和输出上限以模型服务实际配置为准。
        </p>
        <div class="model-toolbar">
          <AppButton type="submit" variant="primary">保存 API</AppButton
          ><AppButton @click="editing = false">取消编辑</AppButton>
        </div>
      </fieldset>
    </form>
    <AppNotice :message="error" />
  </AppDialog>
</template>
