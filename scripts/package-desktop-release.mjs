import { readFile, mkdir, copyFile, writeFile, stat } from 'node:fs/promises';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const [exeArg, outArg] = process.argv.slice(2);
if (!exeArg || !outArg) throw new Error('Usage: node scripts/package-desktop-release.mjs <x64 Release exe> <new output directory>');
const exe = resolve(exeArg), output = resolve(outArg);
if (await stat(output).then(() => true, () => false)) throw new Error('Output already exists; use a new directory.');
const bytes = await readFile(exe);
const pe = bytes.readUInt32LE(0x3c);
if (bytes.subarray(0, 2).toString() !== 'MZ' || bytes.readUInt32LE(pe) !== 0x4550 || bytes.readUInt16LE(pe + 4) !== 0x8664)
  throw new Error('Expected a Windows x64 PE executable.');
if (bytes.readUInt16LE(pe + 24 + 68) !== 2) throw new Error('Expected Windows GUI subsystem.');
for (const marker of ['BFTOOL_TEST_CDP_PORT', 'BFTOOL_TEST_PROFILE', '--remote-debugging-port='])
  if (bytes.includes(Buffer.from(marker))) throw new Error(`Debug-only marker found: ${marker}`);
execFileSync('python', [resolve(root, 'scripts/collect-third-party-notices.py')], { stdio: 'inherit' });
await mkdir(output, { recursive: true });
await copyFile(exe, resolve(output, 'bftool-desktop.exe'));
execFileSync('python', [resolve(root, 'scripts/collect-third-party-notices.py'), '--output', resolve(output, 'LICENSE')], { stdio: 'inherit' });
await copyFile(resolve(root, 'apps/desktop/README.md'), resolve(output, 'README.md'));
const files = ['bftool-desktop.exe', 'LICENSE', 'README.md'];
const sums = await Promise.all(files.map(async name => `${createHash('sha256').update(await readFile(resolve(output, name))).digest('hex')}  ${name}`));
await writeFile(resolve(output, 'SHA256SUMS.txt'), sums.join('\n') + '\n');
console.log(`Packaged x64 Release in ${output}. WebView2 Runtime is required; binary is unsigned and remains a migration candidate.`);
