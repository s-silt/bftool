// Execute the shipped helper with synthetic clipboard results; never access OS clipboard.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';
import ts from 'typescript';
const source=await fs.readFile(new URL('../src/App.vue',import.meta.url),'utf8');
const helper=source.slice(source.indexOf('async function copyText('),source.indexOf('\nonMounted('));
const code=ts.transpileModule(helper,{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
function context(clipboard,execResult=true){
  const nodes=[];
  const box={navigator:{clipboard},copiedKey:{value:'previous'},copyFailure:{value:null},copyRequest:0,setTimeout(){},
    document:{body:{appendChild(el){nodes.push(el)},removeChild(el){nodes.splice(nodes.indexOf(el),1)}},execCommand(){return execResult},createElement(){return {style:{},select(){}}}}};
  vm.runInNewContext(code,box);return {box,nodes};
}
test('fulfilled clipboard write alone reports copied',async()=>{
  const {box}=context({writeText:async()=>{}});
  assert.equal(await box.copyText('synthetic text','sample'),true);
  assert.equal(box.copiedKey.value,'sample');assert.equal(box.copyFailure.value,null);
});
test('rejected clipboard Promise exposes failure and manual-copy content',async()=>{
  const {box}=context({writeText:async()=>{throw Error('synthetic denied')}});
  assert.equal(await box.copyText('synthetic text','sample'),false);
  assert.equal(box.copiedKey.value,null);assert.equal(box.copyFailure.value.text,'synthetic text');
  assert.match(source,/data-copy-failure/);assert.match(source,/重试复制/);assert.match(source,/Ctrl\+C/);
});
test('execCommand false never reports copied and removes temporary control',async()=>{
  const {box,nodes}=context(undefined,false);
  assert.equal(await box.copyText('synthetic text','sample'),false);
  assert.equal(box.copiedKey.value,null);assert.equal(box.copyFailure.value.key,'sample');assert.equal(nodes.length,0);
});
