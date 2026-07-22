import { cp, mkdir, rm } from 'node:fs/promises';
import { build } from 'esbuild';

await rm('dist', { recursive: true, force: true });
await mkdir('dist', { recursive: true });
await build({
  entryPoints: {
    background: 'src/background.ts',
    popup: 'src/popup.ts',
    sidepanel: 'src/sidepanel.ts'
  },
  outdir: 'dist',
  bundle: true,
  format: 'esm',
  platform: 'browser',
  target: 'chrome116',
  sourcemap: true,
  legalComments: 'none'
});
await Promise.all([
  cp('manifest.json', 'dist/manifest.json'),
  cp('src/popup.html', 'dist/popup.html'),
  cp('src/sidepanel.html', 'dist/sidepanel.html'),
  cp('src/ui.css', 'dist/ui.css')
]);
