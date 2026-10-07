<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import type { EntryDto, ErrorDto } from './types';
import { formatBytes } from './model';

const props = defineProps<{ planId: string; count: string }>();
const viewport = ref<HTMLDivElement | null>(null);
const offset = ref(0);
const entries = ref<EntryDto[]>([]);
const error = ref('');
const rowHeight = 44;
const total = computed(() => Number(BigInt(props.count) > 100000000n ? 100000000n : BigInt(props.count)));
let request = 0;

async function load() {
  const ticket = ++request;
  const token = props.planId;
  error.value = '';
  entries.value = [];
  try {
    const rows = await invoke<EntryDto[]>('plan_entries', { planId: token, offset: offset.value, limit: 40 });
    if (ticket === request && token === props.planId) entries.value = rows;
  } catch (e) {
    if (ticket === request) error.value = (e as ErrorDto).message || String(e);
  }
}

watch(() => props.planId, () => {
  if (viewport.value) viewport.value.scrollTop = 0;
  offset.value = 0;
  entries.value = [];
  void load();
}, { immediate: true });

function scroll(e: Event) {
  const start = Math.max(0, Math.floor((e.target as HTMLElement).scrollTop / rowHeight) - 3);
  if (start !== offset.value) {
    offset.value = start;
    void load();
  }
}
</script>

<template>
  <div class="virtual-container">
    <div v-if="error" class="error-panel" role="alert">
      <strong>清单加载失败</strong>
      <p>{{ error }}</p>
    </div>
    <div class="virtual-table-header">
      <span>待备份相对路径</span>
      <span>类型 / 大小</span>
    </div>
    <div ref="viewport" class="virtual-list" @scroll="scroll" tabindex="0" aria-label="只读备份清单">
      <div :style="{ height: `${total * rowHeight}px`, position: 'relative' }">
        <div :style="{ transform: `translateY(${offset * rowHeight}px)` }">
          <div
            v-for="(row, index) in entries"
            :key="`${planId}:${offset + index}`"
            class="entry"
          >
            <span class="entry-path" :title="`${row.source} / ${row.relative_path}`">{{ row.relative_path || '根目录' }}</span>
            <small class="entry-size">{{ row.kind === 'File' ? formatBytes(row.bytes) : '文件夹' }}</small>
          </div>
        </div>
      </div>
    </div>
    <div class="virtual-footer">
      <small>显示窗口内清单 · 共 {{ count }} 项</small>
    </div>
  </div>
</template>
