// Prepare an active synthetic native copy for the PowerShell close-window check.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
const repo = path.resolve(import.meta.dirname, '..');
let pages;
for(let attempt=0;attempt<100;attempt++) {
    for(const address of ['127.0.0.1','[::1]']) {
        try { pages=await (await fetch(`http://${address}:9432/json/list`)).json();if(pages.some(p=>p.url==='http://tauri.localhost/'))break; } catch {}
    }
    if(pages?.some(p=>p.url==='http://tauri.localhost/'))break;
    await new Promise(resolve=>setTimeout(resolve,100));
}
assert.ok(pages,'test WebView2 became available');
const page = pages.find(p => p.url === 'http://tauri.localhost/');
assert.ok(page);
const socket = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
let nextId=0;const pending=new Map();
socket.addEventListener('message',e=>{const value=JSON.parse(e.data);if(value.id){const callback=pending.get(value.id);pending.delete(value.id);value.error?callback.reject(Error(JSON.stringify(value.error))):callback.resolve(value.result);}});
function send(method,params){return new Promise((resolve,reject)=>{const id=++nextId;pending.set(id,{resolve,reject});socket.send(JSON.stringify({id,method,params}));});}
async function evaluate(expression){const result=await send('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(result.exceptionDetails)throw Error(JSON.stringify(result.exceptionDetails));return result.result.value;}
const fixture=path.join(repo,'.build/native-close-fixture-'+Date.now());
const source=path.join(fixture,'窗口关闭测试.bin');const target=path.join(fixture,'目标');
await fs.mkdir(target,{recursive:true});const handle=await fs.open(source,'w');await handle.truncate(512*1024*1024);await handle.close();
const revision=await evaluate(`window.__TAURI_INTERNALS__.invoke('input_revision').then(r=>(BigInt(r)+1n).toString())`);
await evaluate(`window.__TAURI_INTERNALS__.invoke('invalidate_plan',{revision:${JSON.stringify(revision)}})`);
const request={revision,target,sources:[{path:source,kind:'file',recursive:false,suffixes:'',include_extensionless:true}]};
const plan=await evaluate(`window.__TAURI_INTERNALS__.invoke('preview_backup',{request:${JSON.stringify(request)}})`);
const job=await evaluate(`window.__TAURI_INTERNALS__.invoke('start_backup',{revision:${JSON.stringify(revision)},planId:${JSON.stringify(plan.plan_id)}})`);
// Observe actual reporter progress before asking the native window to close.
// This exercises cleanup after execution began, not just cancellation before spawn.
let snapshot;
for(let attempt=0;attempt<600;attempt++) {
    snapshot=await evaluate(`window.__TAURI_INTERNALS__.invoke('job_snapshot')`);
    assert.equal(snapshot.job_id,job);assert.equal(snapshot.terminal,false);
    if(BigInt(snapshot.current_bytes)>0n)break;
    await new Promise(resolve=>setTimeout(resolve,50));
}
assert.ok(BigInt(snapshot.current_bytes)>0n,'core reporter made progress');
await fs.writeFile(path.join(repo,'.build/native-close-evidence.json'),JSON.stringify({fixture,source,target,job,activeSnapshot:snapshot,destination:plan.destinations[0],sourceSize:512*1024*1024},null,2));
console.log('Synthetic copy active; ready for close-window test.');socket.close();
