import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import { parse, compileScript } from '@vue/compiler-sfc';
import ts from 'typescript';
import * as Vue from 'vue';
import { renderToString } from '@vue/server-renderer';
const source = await fs.readFile(new URL('../src/DiagnosticNotice.vue', import.meta.url), 'utf8');
const { descriptor } = parse(source);
const script = compileScript(descriptor, { id: 'error-test', inlineTemplate: true });
let js = ts.transpileModule(script.content, { compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS } }).outputText;
const exports = {};
new Function('require', 'exports', js)(name => { assert.equal(name, 'vue'); return Vue; }, exports);
const component = exports.default;
test('notice renders Chinese first layer and collapsed, escaped full diagnostic', async () => {
  const raw = 'COPY_FAILED C:\\中文长路径\\x.zip <script>alert(1)</script>';
  const html = await renderToString(Vue.createSSRApp(component, { problem: '复制未能完成。', action: '保留源文件和恢复记录。', details: raw }));
  assert.match(html, /复制未能完成/); assert.match(html, /保留源文件和恢复记录/);
  assert.match(html, /<details class="diagnostic-details">/); assert.ok(!/<details[^>]*\sopen/.test(html));
  assert.ok(!html.includes('<script>')); assert.match(html, /&lt;script&gt;/);
  assert.ok(html.indexOf('COPY_FAILED') > html.indexOf('<details'));
  assert.match(html, /复制详情/); assert.match(html, /tabindex="0"/);
});
test('copy feedback is a prop from actual parent copy fulfillment', async () => {
  const html = await renderToString(Vue.createSSRApp(component, { problem: '未完成', action: '检查详情', details: 'raw', copied: true }));
  assert.match(html, />已复制<\/button>/);
  assert.match(source, /\$emit\('copy', details\)/);
});
