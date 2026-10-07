import path from 'node:path';
import { build } from 'vite';
import vue from '@vitejs/plugin-vue';
const repo=path.resolve(import.meta.dirname,'../../..');
const out=process.argv[2];
if(!out||!path.resolve(out).startsWith(path.join(repo,'.build')+path.sep))throw Error('Fixture output must be inside task .build');
await build({configFile:false,root:path.join(repo,'apps/desktop'),define:{'process.env.NODE_ENV':JSON.stringify('production')},plugins:[vue()],resolve:{alias:{'@tauri-apps/api/core':path.join(import.meta.dirname,'virtual-scroll.ipc.ts')}},build:{outDir:path.resolve(out),emptyOutDir:false,lib:{entry:path.join(import.meta.dirname,'virtual-scroll.fixture.ts'),name:'BftoolScrollRegression',formats:['iife'],fileName:()=> 'virtual-scroll-fixture.js'}}});
