<script setup lang="ts">
import { computed, ref } from 'vue'

const props = defineProps<{ locale: 'en' | 'zh' }>()
const model = ref('example-chat')
const stream = ref(false)
const copy = computed(() => props.locale === 'zh'
  ? { model: '示例模型', stream: '使用流式响应', note: '此预览只在浏览器中生成文本，不发送请求。模型名、地址和凭证都是占位符；运行前请替换为部署提供的值。请求可能产生费用。', prompt: '你好' }
  : { model: 'Example model', stream: 'Use streaming response', note: 'This preview generates text in your browser and sends no request. The model, address, and credential are placeholders; replace them with values from your deployment before running. Requests may incur charges.', prompt: 'Hello' })
const command = computed(() => {
  const body = JSON.stringify({ model: model.value, messages: [{ role: 'user', content: copy.value.prompt }], stream: stream.value }, null, 2)
  return `curl${stream.value ? ' -N' : ''} -X POST https://mtc.example.com/v1/chat/completions \\\n  -H "Authorization: Bearer mtc_example_client_key" \\\n  -H "Content-Type: application/json" \\\n  -d '${body}'`
})
</script>

<template>
  <div class="request-example">
    <p>{{ copy.note }}</p>
    <label>
      {{ copy.model }}
      <select v-model="model">
        <option value="example-chat">example-chat</option>
        <option value="example-reasoning">example-reasoning</option>
      </select>
    </label>
    <label><input v-model="stream" type="checkbox"> {{ copy.stream }}</label>
    <pre><code>{{ command }}</code></pre>
  </div>
</template>

<style scoped>
.request-example { border: 1px solid var(--vp-c-divider); border-radius: 8px; padding: 0 16px 16px; }
.request-example label { display: block; margin: 12px 0; }
.request-example select { border: 1px solid var(--vp-c-divider); border-radius: 4px; padding: 4px 8px; margin-left: 8px; background: var(--vp-c-bg); color: var(--vp-c-text-1); }
.request-example input { appearance: auto; margin-right: 4px; }
.request-example pre { overflow-x: auto; margin: 16px 0 0; padding: 16px; border-radius: 6px; background: var(--vp-code-block-bg); }
</style>
