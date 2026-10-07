// Native WebView2 integration driver: no mocked IPC, only synthetic task directories.
import fs from 'node:fs/promises';
import path from 'node:path';
import assert from 'node:assert/strict';
const repo = path.resolve(import.meta.dirname, '..');
const out = path.join(repo, '.build/native-evidence'); await fs.mkdir(out,{recursive:true});
const pages=await (await fetch('http://127.0.0.1:9432/json/list')).json();
const page=pages.find(p=>p.url==='http://tauri.localhost/'); assert.ok(page,'native bundled page');
const ws=new WebSocket(page.webSocketDebuggerUrl); await new Promise((ok,no)=>{ws.addEventListener('open',ok,{once:true});ws.addEventListener('error',no,{once:true});});
let id=0;const waiting=new Map();const events=[];
ws.addEventListener('message',e=>{const r=JSON.parse(e.data);if(r.id){const p=waiting.get(r.id);waiting.delete(r.id);r.error?p?.reject(Error(JSON.stringify(r.error))):p?.resolve(r.result);}else events.push(r);});
function send(method,params={}){return new Promise((resolve,reject)=>{const request=++id;waiting.set(request,{resolve,reject});ws.send(JSON.stringify({id:request,method,params}));});}
async function evaluate(expression){const r=await send('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});if(r.exceptionDetails)throw Error(JSON.stringify(r.exceptionDetails));return r.result.value;}
async function waitFor(expression,timeout=20000){const until=Date.now()+timeout;while(Date.now()<until){if(await evaluate(expression))return;await new Promise(r=>setTimeout(r,50));}throw Error('Timed out: '+expression);}
async function click(text){return evaluate(`(()=>{const b=[...document.querySelectorAll('button')].find(b=>b.textContent.trim()===${JSON.stringify(text)});if(!b||b.disabled)throw Error('missing/disabled button');b.click();return true})()`);}
async function input(label,value){return evaluate(`(()=>{const e=document.querySelector('input[aria-label='+${JSON.stringify(label)}+']');if(!e)throw Error('missing input');e.value=${JSON.stringify(value)};e.dispatchEvent(new Event('input',{bubbles:true}));return true})()`);}
async function shot(name){const r=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:false});await fs.writeFile(path.join(out,name+'.png'),Buffer.from(r.data,'base64'));}
await send('Runtime.enable');await send('Page.enable');await send('Log.enable');
const report={nativeUrl:page.url,backend:'real Tauri IPC',checks:[],screenshots:[],limits:['Physical system picker interaction not exercised','Emulated WebView viewport/scale is not OS DPI certification','No real backup disk or user data used']};
await waitFor(`document.querySelector('h1') && !document.querySelector('.notice')`);
await shot('01-native-empty');report.checks.push('native empty state');
// Short Chinese path plus a >260-character fixture path.
const fixture=path.join(repo,'.build/native-fixture-'+Date.now());await fs.mkdir(fixture,{recursive:true});
const source=path.join(fixture,'中文源目录_'+ '长路径'.repeat(40));const target=path.join(fixture,'中文目标');
await fs.mkdir(path.join(source,'子目录'),{recursive:true});await fs.mkdir(target,{recursive:true});
await fs.writeFile(path.join(source,'甲.ZIP'),'safe payload');await fs.writeFile(path.join(source,'skip.txt'),'not selected');await fs.writeFile(path.join(source,'子目录','乙.zip'),'nested');
await click('输入文件夹路径');await input('源路径',source);await input('目标目录',target);
await evaluate(`(()=>{const e=document.querySelector('.rules input:not([type=checkbox])');e.value='zip';e.dispatchEvent(new Event('input',{bubbles:true}));const c=document.querySelectorAll('.rules input[type=checkbox]')[1];c.checked=false;c.dispatchEvent(new Event('change',{bubbles:true}));})()`);
await waitFor(`!document.querySelectorAll('button')[0].disabled`);
await click('生成预览');await waitFor(`document.querySelector('.primary')`);
assert.equal(await evaluate(`document.querySelector('.stats strong').textContent`),'1');
await assert.rejects(fs.stat(path.join(target,'.bftool-backup')));report.checks.push('Chinese >260 path, shallow suffix filter, read-only preview');
await shot('02-native-preview');
const old=await evaluate(`window.__TAURI_INTERNALS__.invoke('job_snapshot')`);
await evaluate(`(()=>{const e=document.querySelector('.rules input:not([type=checkbox])');e.value='txt';e.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await waitFor(`!document.querySelector('.primary')`);await waitFor(`window.__TAURI_INTERNALS__.invoke('input_revision').then(r=>BigInt(r)>BigInt(${JSON.stringify(old.revision)}))`);
const stale=await evaluate(`window.__TAURI_INTERNALS__.invoke('start_backup',{revision:${JSON.stringify(old.revision)},planId:'invalid-display-token'}).then(()=>null,e=>e)`);assert.equal(stale.code,'STALE_PLAN');report.checks.push('input change invalidates displayed plan; stale revision rejected by real IPC');
await evaluate(`(()=>{const e=document.querySelector('.rules input:not([type=checkbox])');e.value='zip';e.dispatchEvent(new Event('input',{bubbles:true}));})()`);
await click('生成预览');await waitFor(`document.querySelector('.primary')`);
await evaluate(`(()=>{const b=document.querySelector('.primary');b.click();b.click();})()`);
await click('☷ 任务记录');await waitFor(`document.querySelector('.job h2')?.textContent==='已验证并发布'`);
assert.equal(await fs.readFile(path.join(source,'甲.ZIP'),'utf8'),'safe payload');
await click('加载记录');await waitFor(`document.querySelector('.history')`);await click('SHA-256 校验');await waitFor(`document.querySelector('.result strong')?.textContent==='校验完成'`);
assert.equal(await evaluate(`document.querySelectorAll('.history').length`),1);
report.checks.push('real copy, SHA256 verification, source retained, history, task page navigation, duplicate click');await shot('03-native-history-verified');
await click('▣ 文件备份');await input('目标目录',path.join(fixture,'不存在'));
await click('生成预览');await waitFor(`document.querySelector('[role=alert]')`);await shot('04-native-failure');report.checks.push('structured failure state');await input('目标目录',target);
const large=path.join(source,'cancel-large.zip');const handle=await fs.open(large,'w');await handle.truncate(256*1024*1024);await handle.close();
await click('生成预览');await waitFor(`document.querySelector('.primary')`,60000);await click('开始复制 →');
await waitFor(`document.querySelector('.job button')?.textContent==='安全取消'`);await click('安全取消');
await waitFor(`document.querySelector('.job h2')?.textContent==='已取消'`,60000);
assert.equal((await fs.stat(large)).size,256*1024*1024);await shot('08-native-cancelled');report.checks.push('real UI cancellation waits for backend terminal, source preserved');
for(let index=0;index<100;index++)await fs.writeFile(path.join(source,`virtual_${String(index).padStart(3,'0')}.zip`),'synthetic');
await click('生成预览');await waitFor(`document.querySelector('.virtual-list .entry')`,60000);
await evaluate(`(()=>{const e=document.querySelector('.virtual-list');e.scrollTop=44*60;e.dispatchEvent(new Event('scroll'));})()`);
await waitFor(`[...document.querySelectorAll('.virtual-list .entry span')].some(e=>e.textContent.startsWith('virtual_05'))`);
assert.ok(await evaluate(`document.querySelectorAll('.virtual-list .entry').length<=40`));
await evaluate(`document.querySelector('.virtual-list').scrollIntoView({block:'center'})`);
await shot('09-native-virtual-list');report.checks.push('bounded virtual list page and real scrolling');
for(const [width,height,scale,name] of [[420,780,1,'05-native-narrow'],[800,900,1.5,'06-native-scale150'],[1080,800,2,'07-native-scale200']]) {
  await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:scale,mobile:false});await shot(name);report.screenshots.push(name+'.png');
  assert.ok(await evaluate(`document.documentElement.scrollWidth<=innerWidth+1`),'no horizontal viewport overflow');
}
await send('Emulation.clearDeviceMetricsOverride');
const forbidden=await evaluate(`window.__TAURI_INTERNALS__.invoke('plugin:shell|execute',{program:'cmd'}).then(()=>null,e=>String(e))`);assert.ok(forbidden);report.checks.push('shell command unavailable');
try{await send('Page.navigate',{url:'https://example.com/'});}catch{}
await new Promise(r=>setTimeout(r,200));assert.equal(await evaluate('location.origin'),'http://tauri.localhost');report.checks.push('top-level remote navigation denied');
report.runtimeExceptions=events.filter(e=>e.method==='Runtime.exceptionThrown');report.consoleErrors=events.filter(e=>e.method==='Log.entryAdded'&&e.params.entry.level==='error');
report.screenshots=(await fs.readdir(out)).filter(f=>f.endsWith('.png')).sort();
report.fixture=fixture;await fs.writeFile(path.join(out,'report.json'),JSON.stringify(report,null,2));
ws.close();console.log(JSON.stringify(report,null,2));
