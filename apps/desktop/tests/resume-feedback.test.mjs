// Execute the shipped recovery functions with controlled IPC completion ordering.
import {test} from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import vm from 'node:vm';
import ts from 'typescript';
const source=await fs.readFile(new URL('../src/App.vue',import.meta.url),'utf8');
function fn(start,end){return source.slice(source.indexOf(start),source.indexOf(end,source.indexOf(start)));}
const code=ts.transpileModule(fn('function fail(','\nfunction changed(')+fn('async function poll(','\nasync function preview(')+fn('async function resume(','\nasync function verify('),{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
test('same-turn recovery double click admits one request and retains truthful terminal feedback',async()=>{
 let resolveResume,calls=0;
 const pending=new Promise(r=>resolveResume=r),ref=value=>({value});
 const snapshot={job_id:'new-job',terminal:true,phase:'completed',error:null};
 const box={busy:ref(false),revision:ref('1'),error:ref(null),expectedJob:ref(null),snapshot:ref(null),plan:ref({}),history:ref([]),verification:ref(null),native:true,polling:false,invalidation:Promise.resolve(),acceptSnapshot:()=>true,
 invoke:async command=>{if(command==='resume_backup'){calls++;return pending;}if(command==='job_snapshot')return snapshot;throw Error(command);}};
 box.running={get value(){return box.busy.value||!!box.snapshot.value&&!box.snapshot.value.terminal;}};
 vm.runInNewContext(code,box);
 const first=box.resume({recovery_id:'opaque-token'}),second=box.resume({recovery_id:'opaque-token'});
 await second;await Promise.resolve();assert.equal(calls,1);assert.equal(box.busy.value,true);assert.equal(box.error.value,null);
 resolveResume('new-job');await first;assert.equal(box.busy.value,false);assert.equal(box.snapshot.value.phase,'completed');assert.equal(box.error.value,null);
});
