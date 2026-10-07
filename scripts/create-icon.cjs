// Reproducible 32px application icon, required by tauri-build on Windows.
const fs = require('node:fs');
const path = require('node:path');
const n = 32, maskBytes = 128, payloadSize = 40 + n*n*4 + maskBytes;
const buffer = Buffer.alloc(22+payloadSize);
buffer.writeUInt16LE(1,2); buffer.writeUInt16LE(1,4); buffer[6]=n; buffer[7]=n;
buffer.writeUInt16LE(1,10); buffer.writeUInt16LE(32,12); buffer.writeUInt32LE(payloadSize,14); buffer.writeUInt32LE(22,18);
buffer.writeUInt32LE(40,22); buffer.writeInt32LE(n,26); buffer.writeInt32LE(n*2,30); buffer.writeUInt16LE(1,34); buffer.writeUInt16LE(32,36);
const glyphs=['10000','10000','11110','10001','10001','10001','11110'];
for(let y=0;y<n;y++)for(let x=0;x<n;x++) {
  const i=62+((n-1-y)*n+x)*4;
  const gx=Math.floor((x-8)/3), gy=Math.floor((y-5)/3);
  const white=gx>=0&&gx<5&&gy>=0&&gy<7&&glyphs[gy][gx]==='1';
  buffer[i]=white?255:220;buffer[i+1]=white?255:103;buffer[i+2]=white?255:40;buffer[i+3]=255;
}
const target=path.join(__dirname,'../apps/desktop/src-tauri/icons/icon.ico');
fs.mkdirSync(path.dirname(target),{recursive:true});fs.writeFileSync(target,buffer);
