<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import type { ErrorDto, HistoryDto, PlanDto, Snapshot, SourceInput, VerificationDto } from './types';
import { acceptSnapshot, bumpRevision, formatBytes, phases, progressPercent } from './model';
import VirtualEntries from './VirtualEntries.vue';
import DiagnosticNotice from './DiagnosticNotice.vue';
import { errorDiagnostic, presentError, resultTitle } from './errorPresentation';

const page = ref<'backup' | 'tasks'>('backup');
const sources = ref<SourceInput[]>([]);
const target = ref('');
const revision = ref('0');
const plan = ref<PlanDto | null>(null);
const snapshot = ref<Snapshot | null>(null);
const error = ref<ErrorDto | null>(null);
const busy = ref(false);
const expectedJob = ref<string | null>(null);
const history = ref<HistoryDto[]>([]);
const verification = ref<VerificationDto | null>(null);
const native = !!(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
const copiedKey = ref<string | null>(null);
const copyFailure = ref<{ text: string; key: string } | null>(null);
let copyRequest = 0;

let invalidation = Promise.resolve();
let polling = false;
let timer: ReturnType<typeof setInterval>;

const running = computed(() => busy.value || !!snapshot.value && !snapshot.value.terminal);
const progress = computed(() => snapshot.value ? progressPercent(snapshot.value) : null);
const errorView = computed(() => error.value ? presentError(error.value) : null);
const errorDetails = computed(() => error.value ? errorDiagnostic(error.value, snapshot.value) : '');

function fail(e: unknown) {
  error.value = typeof e === 'object' && e && 'code' in e
    ? e as ErrorDto
    : { code: 'IPC_FAILED', message: String(e), operation: 'ipc', retry: '检查桌面运行环境后重试' };
}

function changed() {
  revision.value = bumpRevision(revision.value);
  plan.value = null;
  verification.value = null;
  history.value = [];
  error.value = null;
  const value = revision.value;
  if (native) {
    invalidation = invalidation
      .catch(() => {})
      .then(() => invoke<void>('invalidate_plan', { revision: value }))
      .catch(e => {
        if (value === revision.value) fail(e);
      });
  }
}

function addPath(kind: 'file' | 'directory', path = '') {
  sources.value.push({ kind, path, recursive: false, suffixes: '', include_extensionless: true });
  changed();
}

async function pick(kind: 'file' | 'directory' | 'target') {
  try {
    const paths = await invoke<string[]>('choose_paths', { kind });
    if (!paths.length) return;
    if (kind === 'target') {
      target.value = paths[0]!;
      changed();
    } else {
      for (const path of paths) addPath(kind, path);
    }
  } catch (e) {
    fail(e);
  }
}

async function poll() {
  if (polling || !native) return;
  polling = true;
  try {
    const next = await invoke<Snapshot | null>('job_snapshot');
    if (next && acceptSnapshot(snapshot.value, next, expectedJob.value)) {
      snapshot.value = next;
      if (next.terminal) {
        expectedJob.value = null;
        if (next.error) error.value = next.error;
      }
    }
  } catch (e) {
    fail(e);
  } finally {
    polling = false;
  }
}

async function preview() {
  busy.value = true;
  error.value = null;
  expectedJob.value = null;
  const rev = revision.value;
  try {
    await invalidation;
    const result = await invoke<PlanDto>('preview_backup', {
      request: { revision: rev, target: target.value, sources: sources.value }
    });
    if (rev === revision.value) plan.value = result;
  } catch (e) {
    fail(e);
  } finally {
    busy.value = false;
    await poll();
  }
}

async function start() {
  if (!plan.value) return;
  busy.value = true;
  error.value = null;
  try {
    const job = await invoke<string>('start_backup', { revision: revision.value, planId: plan.value.plan_id });
    expectedJob.value = job;
    snapshot.value = null;
    plan.value = null;
    await poll();
  } catch (e) {
    fail(e);
  } finally {
    busy.value = false;
  }
}

async function cancel() {
  if (!snapshot.value) return;
  try {
    await invoke('cancel_job', { jobId: snapshot.value.job_id });
    await poll();
  } catch (e) {
    fail(e);
  }
}

async function records() {
  busy.value = true;
  error.value = null;
  expectedJob.value = null;
  history.value = [];
  const rev = revision.value;
  const selected = target.value;
  try {
    const result = await invoke<HistoryDto[]>('backup_history', { target: selected });
    if (rev === revision.value && selected === target.value) history.value = result;
  } catch (e) {
    fail(e);
  } finally {
    busy.value = false;
    await poll();
  }
}

async function resume(record: HistoryDto) {
  if (running.value) return;
  if (!record.recovery_id) return;
  busy.value = true;
  error.value = null;
  const rev = revision.value;
  try {
    await invalidation;
    if (rev !== revision.value) return;
    const job = await invoke<string>('resume_backup', { revision: rev, recoveryId: record.recovery_id });
    expectedJob.value = job;
    snapshot.value = null;
    plan.value = null;
    history.value = [];
    verification.value = null;
    await poll();
  } catch (e) {
    fail(e);
  } finally {
    busy.value = false;
  }
}

async function verify() {
  busy.value = true;
  error.value = null;
  expectedJob.value = null;
  const rev = revision.value;
  const selected = target.value;
  try {
    const result = await invoke<VerificationDto>('verify_backup', { target: selected });
    if (rev === revision.value && selected === target.value) verification.value = result;
  } catch (e) {
    fail(e);
  } finally {
    busy.value = false;
    await poll();
  }
}

async function copyText(text: string, key: string) {
  if (!text) return false;
  const ticket = ++copyRequest;
  copiedKey.value = null;
  copyFailure.value = null;
  try {
    if (navigator?.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const el = document.createElement('textarea');
      el.value = text;
      el.style.position = 'fixed';
      el.style.opacity = '0';
      document.body.appendChild(el);
      try {
        el.select();
        if (!document.execCommand('copy')) throw new Error('Clipboard copy failed');
      } finally {
        document.body.removeChild(el);
      }
    }
    if (ticket === copyRequest) {
      copiedKey.value = key;
      setTimeout(() => {
        if (ticket === copyRequest) copiedKey.value = null;
      }, 1600);
    }
    return true;
  } catch {
    if (ticket === copyRequest) copyFailure.value = { text, key };
    return false;
  }
}

onMounted(async () => {
  if (native) {
    busy.value = true;
    try {
      revision.value = await invoke<string>('input_revision');
      await poll();
    } catch (e) {
      fail(e);
    } finally {
      busy.value = false;
    }
    timer = setInterval(() => void poll(), 250);
  }
});

onUnmounted(() => clearInterval(timer));
</script>

<template>
  <div class="app-shell">
    <aside>
      <div class="brand">
        <span class="brand-mark">bf</span>
        <div class="brand-text">
          <span class="brand-name">bftool</span>
          <small>文件备份 · 桌面候选</small>
        </div>
      </div>

      <nav aria-label="主导航">
        <button type="button" :class="{ selected: page === 'backup' }" @click="page = 'backup'">▣ 文件备份</button>
        <button type="button" :class="{ selected: page === 'tasks' }" @click="page = 'tasks'">☷ 任务记录</button>
      </nav>

      <div class="engine-note">
        <div class="engine-status">
          <span class="dot"></span>
          <span>Rust core · COPY 引擎</span>
        </div>
        <small>
          · 保留源文件（只读不删）<br>
          · KeepBoth 同名保留两份<br>
          · SHA-256 完整性校验
        </small>
      </div>
    </aside>

    <main>
      <header>
        <div>
          <p class="eyebrow">本地文件 · 安全备份</p>
          <h1>{{ page === 'backup' ? '把重要文件，放心留一份' : '查看备份结果' }}</h1>
          <p class="subtitle">
            {{
              page === 'backup'
                ? '选择源文件和目标目录，先预览，再复制。'
                : '后台任务会继续运行，切换页面不会丢失进度。'
            }}
          </p>
        </div>
        <div class="header-tags">
          <span class="pill pill-safe">防静默覆盖</span>
          <span class="badge">第一迁移切片</span>
        </div>
      </header>

      <div v-if="!native" class="notice" role="status">
        <div class="notice-title">浏览器预览模式</div>
        <p>当前运行在浏览器环境，仅提供界面交互预览。真实文件对话框选择、核心复制与 SHA-256 校验须从 Tauri 桌面程序运行。</p>
      </div>

      <div v-if="copyFailure" class="error-panel" role="alert" data-copy-failure>
        <strong>复制失败</strong>
        <p class="error-action">请重试，或选中下方内容后按 Ctrl+C 手动复制。</p>
        <textarea
          :value="copyFailure.text"
          readonly
          rows="3"
          style="width: 100%; box-sizing: border-box; font: inherit; resize: vertical"
          aria-label="手动复制内容"
          @focus="($event.target as HTMLTextAreaElement).select()"
        ></textarea>
        <div class="actions">
          <button type="button" @click="copyText(copyFailure.text, copyFailure.key)">重试复制</button>
          <button type="button" @click="copyFailure = null">关闭提示</button>
        </div>
      </div>

      <DiagnosticNotice v-if="errorView" :problem="errorView.problem" :action="errorView.action"
        :details="errorDetails" :copied="copiedKey === 'error-details'"
        @copy="copyText($event, 'error-details')" />

      <!-- 当前任务执行中或终态卡片 -->
      <section
        v-if="snapshot && snapshot.phase !== 'preview_ready'"
        class="card job"
        aria-live="polite"
      >
        <div class="section-heading">
          <div class="job-status-line">
            <span class="status-pulse" :class="{ 'status-done': snapshot.terminal && snapshot.phase === 'completed' }" :style="snapshot.terminal && snapshot.phase !== 'completed' ? { background: snapshot.phase === 'failed' ? '#dc2626' : '#94a3b8', animation: 'none' } : undefined"></span>
            <h2>{{ Object.hasOwn(phases, snapshot.phase) ? phases[snapshot.phase] : '任务状态待核实' }}</h2>
          </div>
          <button v-if="!snapshot.terminal" @click="cancel" :disabled="snapshot.cancel_requested" class="btn-cancel">{{ snapshot.cancel_requested ? '等待安全结束…' : '安全取消' }}</button>
        </div>

        <div v-if="snapshot.cancel_requested && !snapshot.terminal" class="cancel-notice">
          <span>⏳ 已发出取消请求，正在等待后台安全事务收尾并保留断点元数据…</span>
        </div>

        <div class="path-display-box" v-if="snapshot.label && !snapshot.terminal">
          <span class="path-tag">当前对象</span>
          <p class="path" :title="snapshot.label">{{ snapshot.label }}</p>
          <button
            type="button"
            class="btn-copy-inline"
            @click="copyText(snapshot.label, 'cur-label')"
            :title="'复制完整路径'"
          >
            {{ copiedKey === 'cur-label' ? '已复制 ✓' : '复制' }}
          </button>
        </div>
        <p v-else-if="!snapshot.terminal" class="path">等待后台报告真实进度</p>
        <details v-else class="terminal-details">
          <summary>查看任务详情</summary>
          <pre>{{ JSON.stringify(snapshot, null, 2) }}</pre>
          <button type="button" @click="copyText(JSON.stringify(snapshot, null, 2), 'task-details')">{{ copiedKey === 'task-details' ? '已复制' : '复制详情' }}</button>
        </details>

        <!-- 进度条与数据量统计 -->
        <div class="progress-wrap">
          <progress v-if="progress !== null" :value="progress" max="100"></progress>
          <progress v-else-if="!snapshot.terminal"></progress>

          <div class="progress-bar-custom" v-if="progress !== null">
            <div class="progress-fill" :style="{ width: `${progress}%` }"></div>
          </div>
        </div>

        <div class="job-metrics" v-if="snapshot.total_bytes !== '0'">
          <span>当前文件传输: <strong>{{ formatBytes(snapshot.current_bytes) }}</strong> / {{ formatBytes(snapshot.total_bytes) }}</span>
          <span class="job-pct" v-if="progress !== null">{{ progress }}%</span>
        </div>

        <!-- 任务结果 -->
        <div
          v-for="result in snapshot.results"
          :key="result.core_job_id"
          class="result"
          :class="{ 'result-success': result.published && result.outcome === 'Completed', 'result-warning': !(result.published && result.outcome === 'Completed') }"
        >
          <div class="result-title-bar">
            <strong>{{ resultTitle(result) }}</strong>
          </div>
          <div class="result-details">
            <span v-if="result.skipped_verified && result.skipped_verified !== '0'">复用已核验: <strong>{{ result.skipped_verified }}</strong> 文件</span>
            <span>已复制: <strong>{{ result.copied }}</strong> 文件</span>
            <span>已校验: <strong>{{ result.verified }}</strong> 文件</span>
            <span>数据量: <strong>{{ formatBytes(result.bytes) }}</strong></span>
          </div>
          <DiagnosticNotice v-if="result.issues.length || !result.published || result.outcome !== 'Completed'"
            :problem="resultTitle(result)" action="保留源文件和恢复记录，查看详情后再处理。"
            :details="JSON.stringify({ job_id: snapshot.job_id, phase: snapshot.phase, result }, null, 2)"
            :copied="copiedKey === 'res-' + result.core_job_id" @copy="copyText($event, 'res-' + result.core_job_id)" />
        </div>
      </section>

      <!-- 备份主流程页面 -->
      <template v-if="page === 'backup'">
        <!-- 步骤 01：选择备份源 -->
        <section class="card">
          <div class="section-heading">
            <div class="step-title">
              <span class="step">01</span>
              <div>
                <h2>选择备份源</h2>
                <small class="step-desc">添加需要备份的文件或整个文件夹，原始文件始终保留</small>
              </div>
            </div>
            <div class="actions">
              <button :disabled="running || !native" @click="pick('file')" title="通过系统对话框选择文件">＋ 文件</button>
              <button :disabled="running || !native" @click="pick('directory')" title="通过系统对话框选择文件夹">＋ 文件夹</button>
            </div>
          </div>

          <div v-if="!sources.length" class="empty">
            <div class="empty-icon">📂</div>
            <h3>从一份文件开始</h3>
            <p>点击上方按钮选择文件或文件夹，也可以在下方直接输入本地绝对路径。</p>
            <div class="actions">
              <button :disabled="running" @click="addPath('file')">输入文件路径</button>
              <button :disabled="running" @click="addPath('directory')">输入文件夹路径</button>
            </div>
          </div>

          <div v-else class="source-list">
            <article
              v-for="(source, index) in sources"
              :key="index"
              class="source"
            >
              <div class="source-line">
                <span class="kind-badge" :class="source.kind">
                  {{ source.kind === 'file' ? '文件' : '文件夹' }}
                </span>
                <input
                  v-model="source.path"
                  :disabled="running"
                  aria-label="源路径"
                  placeholder="绝对路径，支持中文与超长路径"
                  :title="source.path"
                  @input="changed"
                >
                <button
                  type="button"
                  class="btn-copy"
                  :disabled="!source.path"
                  @click="copyText(source.path, 'src-' + index)"
                  title="复制此源路径"
                >
                  {{ copiedKey === 'src-' + index ? '已复制' : '复制' }}
                </button>
                <button
                  class="btn-remove"
                  :disabled="running"
                  @click="sources.splice(index, 1); changed()"
                  aria-label="移除源"
                  title="移除此项"
                >×</button>
              </div>

              <!-- 针对文件夹的筛选规则 -->
              <div v-if="source.kind === 'directory'" class="rules">
                <div class="rules-header">
                  <span class="rules-tag">目录选项</span>
                  <small>每个文件夹拥有独立过滤设置</small>
                </div>
                <div class="rules-body">
                  <label class="rule-suffix">
                    <span class="rule-label">后缀筛选</span>
                    <input
                      v-model="source.suffixes"
                      :disabled="running"
                      placeholder="全部；例如 zip, tar"
                      title="多个后缀用英文逗号分隔，留空备份所有格式"
                      @input="changed"
                    >
                  </label>
                  <label class="rule-checkbox">
                    <input
                      type="checkbox"
                      v-model="source.recursive"
                      :disabled="running"
                      @change="changed"
                    >
                    <span>包含子目录</span>
                    <small class="tip-sub">(默认关闭，仅复制当前层文件)</small>
                  </label>
                  <label class="rule-checkbox">
                    <input
                      type="checkbox"
                      v-model="source.include_extensionless"
                      :disabled="running"
                      @change="changed"
                    >
                    <span>包含无后缀文件</span>
                  </label>
                </div>
              </div>
            </article>
          </div>
        </section>

        <!-- 步骤 02：选择目标目录 -->
        <section class="card">
          <div class="section-heading">
            <div class="step-title">
              <span class="step">02</span>
              <div>
                <h2>选择目标目录</h2>
                <small class="step-desc">指定机械备份盘或外部存储目录</small>
              </div>
            </div>
            <span class="pill">同名保留两份</span>
          </div>

          <div class="target-box">
            <div class="target-line">
              <input
                v-model="target"
                :disabled="running"
                placeholder="选择一个已存在的普通目录（如 D:\Backup）"
                aria-label="目标目录"
                :title="target"
                @input="changed"
              >
              <button
                type="button"
                class="btn-copy"
                :disabled="!target"
                @click="copyText(target, 'target-path')"
                title="复制目标路径"
              >
                {{ copiedKey === 'target-path' ? '已复制' : '复制' }}
              </button>
              <button
                :disabled="running || !native"
                @click="pick('target')"
                title="选择已存在的文件夹"
              >浏览…</button>
            </div>
            <div class="target-hint">
              <span class="hint-tag">安全机制</span>目标目录必须真实存在。遇到已有同名文件时，引擎会自动命名保留两份（KeepBoth），绝不静默覆盖已有文件。
            </div>
          </div>
        </section>

        <!-- 步骤 03：只读预览 -->
        <section class="card">
          <div class="section-heading">
            <div class="step-title">
              <span class="step">03</span>
              <div>
                <h2>只读预览</h2>
                <small class="step-desc">核对真实文件清单与预期目标路径，不进行任何写入</small>
              </div>
            </div>
            <button
              :disabled="running || !native || !sources.length || !target"
              @click="preview"
              class="btn-preview"
            >生成预览</button>
          </div>

          <div v-if="!plan" class="preview-empty">
            <p class="muted">配置源路径与目标目录后，点击右上角“生成预览”。系统将读取并校验源文件生成只读计划，绝不会向目标目录写入任何数据。</p>
          </div>

          <template v-else>
            <div class="stats">
              <div class="stat-card">
                <span class="stat-title">待备份文件</span>
                <strong>{{ plan.files }}</strong>
                <small>项可复制文件</small>
              </div>
              <div class="stat-card">
                <span class="stat-title">待复制数据</span>
                <strong>{{ formatBytes(plan.bytes) }}</strong>
                <small>实际存储占用</small>
              </div>
              <div class="stat-card">
                <span class="stat-title">涉及目录</span>
                <strong>{{ plan.directories }}</strong>
                <small>层级与目标子项</small>
              </div>
            </div>

            <div class="destinations-card">
              <div class="destinations-title">
                <span>预期写入目标</span>
                <button
                  type="button"
                  class="btn-copy-mini"
                  @click="copyText(plan.destinations.join('\n'), 'dest-list')"
                >
                  {{ copiedKey === 'dest-list' ? '已复制' : '复制目标列表' }}
                </button>
              </div>
              <div class="dest-list-wrap">
                <p
                  v-for="(dest, dIdx) in plan.destinations"
                  :key="dIdx"
                  class="path path-dest"
                  :title="dest"
                >
                  {{ dest }}
                </p>
              </div>
            </div>

            <DiagnosticNotice v-if="plan.issues.length" problem="预览发现需要确认的项目。"
              action="请展开详情检查，再决定是否开始复制。" :details="JSON.stringify(plan.issues, null, 2)"
              :copied="copiedKey === 'plan-issues'" @copy="copyText($event, 'plan-issues')" />

            <VirtualEntries :plan-id="plan.plan_id" :count="plan.entry_count" />

            <div class="footer-action">
              <div class="footer-tip">
                <span class="tip-dot">i</span>
                <small>输入修改后，需要重新预览。</small>
              </div>
              <button class="primary" :disabled="running || !native" @click="start">开始复制 →</button>
            </div>
          </template>
        </section>
      </template>

      <!-- 任务记录页面 -->
      <section v-else class="card tasks-card">
        <div class="section-heading">
          <div>
            <h2>目标目录中的任务记录</h2>
            <small class="step-desc">读取现有目标目录的元数据日志（Journal 与 Manifest）并执行 SHA-256 完整性检验</small>
          </div>
          <div class="actions">
            <button :disabled="running || !target || !native" @click="records">加载记录</button>
            <button :disabled="running || !target || !native" @click="verify">SHA-256 校验</button>
          </div>
        </div>

        <p class="muted">重启后重新选择原目标目录并加载记录。未完成任务可检查并继续；恢复前会重新验证源文件、目标身份及恢复元数据，异常证据会保留并拒绝执行。</p>
        <div class="target-banner">
          <span class="banner-label">目标目录:</span>
          <p class="path" :title="target">{{ target || '请先在文件备份页选择目标目录' }}</p>
          <button
            v-if="target"
            type="button"
            class="btn-copy-inline"
            @click="copyText(target, 'task-target')"
          >
            {{ copiedKey === 'task-target' ? '已复制 ✓' : '复制路径' }}
          </button>
        </div>

        <div v-if="!history.length" class="empty compact">
          <div class="empty-icon">📋</div>
          <h3>还没有载入任务记录</h3>
          <p>点击右上角“加载记录”，读取目标目录现有的备份历史与事务状态。</p>
        </div>

        <div v-else class="history-list">
          <article
            v-for="record in history"
            :key="record.core_job_id"
            class="history"
          >
            <div class="history-top">
              <div class="history-status">
                <span class="status-badge" :class="record.completed ? 'status-ok' : 'status-pending'">
                  {{ record.completed ? '已完成' : record.state }}
                </span>
                <strong class="history-dest" :title="record.destination">{{ record.destination }}</strong>
              </div>
              <span class="history-size">{{ formatBytes(record.bytes) }}</span>
            </div>

            <div v-if="record.recovery_id" class="actions">
              <button type="button" :disabled="running || !native" @click="resume(record)">检查并继续</button>
              <small class="muted">{{ record.state }} · 继续原恢复记录，保留源文件</small>
            </div>
            <div class="history-source-box">
              <span class="source-tag">源:</span>
              <p class="path" :title="record.source">{{ record.source }}</p>
              <button
                type="button"
                class="btn-copy-mini"
                @click="copyText(record.source, 'hsrc-' + record.core_job_id)"
                title="复制源路径"
              >
                {{ copiedKey === 'hsrc-' + record.core_job_id ? '已复制' : '复制' }}
              </button>
            </div>

            <div class="history-meta">
              <span class="meta-tag">{{ record.recursive ? '递归规则' : '当前层规则' }}</span>
              <small class="history-job-id" :title="record.core_job_id">
                作业ID: <code>{{ record.core_job_id }}</code>
              </small>
              <button
                type="button"
                class="btn-copy-mini"
                @click="copyText(record.core_job_id, 'hjob-' + record.core_job_id)"
                title="复制作业ID"
              >
                {{ copiedKey === 'hjob-' + record.core_job_id ? '已复制' : '复制' }}
              </button>
            </div>
          </article>
        </div>

        <!-- 校验结果面板 -->
        <div v-if="verification" class="verification-panel">
          <div class="verification-header">
            <span class="v-icon">🔍</span>
            <div class="result">
              <strong>{{
                verification.cancelled
                  ? '校验已取消'
                  : verification.bad !== '0'
                    ? '发现校验问题'
                    : verification.size_only !== '0'
                      ? '存在仅大小校验项'
                      : '校验完成'
              }}</strong>
            </div>
          </div>

          <div class="verify-stats-grid">
            <div class="verify-stat-item">
              <span>已检查文件</span>
              <strong>{{ verification.checked }}</strong>
            </div>
            <div class="verify-stat-item" :class="{ 'has-issues': verification.bad !== '0' }">
              <span>问题项目</span>
              <strong>{{ verification.bad }}</strong>
            </div>
            <div class="verify-stat-item">
              <span>仅大小校验</span>
              <strong>{{ verification.size_only }}</strong>
            </div>
            <div class="verify-stat-item">
              <span>额外文件</span>
              <strong>{{ verification.extras }}</strong>
            </div>
          </div>

          <DiagnosticNotice v-if="verification.issues && verification.issues.length" problem="校验有需要检查的项目。"
            action="请保留源文件，展开详情核对；不要将本次结果视为完整校验通过。"
            :details="JSON.stringify(verification, null, 2)" :copied="copiedKey === 'verification-details'"
            @copy="copyText($event, 'verification-details')" />
        </div>
      </section>

      <footer>
        <span>候选界面 · 使用现有 Rust 备份事务与恢复元数据</span>
      </footer>
    </main>
  </div>
</template>
