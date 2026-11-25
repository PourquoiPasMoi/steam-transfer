// Set the app version everywhere it lives: node scripts/bump.mjs 2.0.1
import { readFileSync, writeFileSync } from 'node:fs';

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(version || '')) {
  console.error('Usage: node scripts/bump.mjs <major.minor.patch>');
  process.exit(1);
}

const edit = (file, fn) => writeFileSync(file, fn(readFileSync(file, 'utf8')));
edit('package.json', (s) => s.replace(/"version": "[^"]+"/, `"version": "${version}"`));
edit('src-tauri/tauri.conf.json', (s) => s.replace(/"version": "[^"]+"/, `"version": "${version}"`));
edit('src-tauri/Cargo.toml', (s) => s.replace(/^version = "[^"]+"/m, `version = "${version}"`));
edit('ui/index.html', (s) => s.replace(/>v\d+\.\d+\.\d+</, `>v${version}<`));
console.log(`Version set to ${version}. Now: git commit -am "v${version}" && git tag v${version} && git push --follow-tags`);
