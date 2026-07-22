import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdir, readFile, readdir, stat, writeFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
import yazl from 'yazl';
import './build.mjs';

async function files(root, dir = root) {
  const entries = await readdir(dir);
  const result = [];
  for (const entry of entries.sort()) {
    const path = join(dir, entry);
    if ((await stat(path)).isDirectory()) result.push(...await files(root, path));
    else result.push({ path, name: relative(root, path) });
  }
  return result;
}

await mkdir('artifacts', { recursive: true });
const output = 'artifacts/tinyflows-chrome-extension-0.1.0.zip';
const zip = new yazl.ZipFile();
for (const file of await files('dist')) {
  zip.addFile(file.path, file.name, { mtime: new Date('1980-01-01T00:00:00Z'), mode: 0o100644 });
}
zip.end();
await new Promise((resolve, reject) => {
  zip.outputStream.pipe(createWriteStream(output)).on('close', resolve).on('error', reject);
});
const digest = createHash('sha256').update(await readFile(output)).digest('hex');
await writeFile(`${output}.sha256`, `${digest}  ${output.split('/').at(-1)}\n`);
console.log(`${output}\nsha256 ${digest}`);
