<script setup lang="ts">
defineProps<{ problem: string; action: string; details: string; copied?: boolean }>();
defineEmits<{ copy: [text: string] }>();
</script>

<template>
  <div class="error-panel diagnostic-notice" role="alert">
    <strong class="error-problem">{{ problem }}</strong>
    <p class="error-action">{{ action }}</p>
    <details class="diagnostic-details">
      <summary>查看详情</summary>
      <div class="diagnostic-toolbar">
        <button type="button" @click="$emit('copy', details)">{{ copied ? '已复制' : '复制详情' }}</button>
      </div>
      <pre tabindex="0" aria-label="完整诊断详情">{{ details }}</pre>
    </details>
  </div>
</template>

<style scoped>
.diagnostic-notice { min-width: 0; }
.error-problem { display: block; line-height: 1.6; }
.diagnostic-details { margin-top: 10px; min-width: 0; }
summary { cursor: pointer; width: fit-content; font-size: 13px; }
summary:focus-visible, pre:focus-visible { outline: 2px solid #dc2626; outline-offset: 3px; }
.diagnostic-toolbar { margin: 10px 0 6px; }
pre { margin: 0; padding: 10px; border-radius: 6px; background: #fff; max-height: 280px; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; font-size: 12px; line-height: 1.6; }
</style>
