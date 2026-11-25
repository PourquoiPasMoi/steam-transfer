// Build on this PC and publish a GitHub Release that installed copies update from.
//
//   node scripts/release.mjs 2.0.2 "What changed"    bump, build, sign, commit, tag, push, upload
//   node scripts/release.mjs --publish-only "Notes"   upload the build that already exists for the current version
//
// Needs: the updater signing key at %USERPROFILE%\.tauri\steam-transfer.key (or TAURI_SIGNING_PRIVATE_KEY set),
// and `gh` signed in to the account that owns the `origin` repository.
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

const args = process.argv.slice(2);
const publishOnly = args[0] === '--publish-only';
if (publishOnly) args.shift();
const run = (cmd, cmdArgs, opts = {}) => execFileSync(cmd, cmdArgs, { stdio: 'inherit', ...opts });
const out = (cmd, cmdArgs, opts = {}) => execFileSync(cmd, cmdArgs, { encoding: 'utf8', ...opts }).trim();

let version;
if (publishOnly) {
  version = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8')).version;
} else {
  version = args.shift();
  if (!/^\d+\.\d+\.\d+$/.test(version || '')) {
    console.error('Usage: node scripts/release.mjs <major.minor.patch> "release notes"');
    process.exit(1);
  }
}
const notes = args.join(' ') || `Steam Transfer v${version}`;
const tag = `v${version}`;

// Which GitHub account/repo: taken from the origin remote, e.g. https://github.com/<owner>/<repo>.git
const remote = out('git', ['remote', 'get-url', 'origin']);
const [, owner, repo] = remote.match(/github\.com[/:]([^/]+)\/([^/.]+)/) || [];
if (!owner) throw new Error(`Can't read the GitHub repo from origin: ${remote}`);
const ghEnv = { ...process.env, GH_TOKEN: process.env.GH_TOKEN || out('gh', ['auth', 'token', '--user', owner]) };

if (!publishOnly) {
  const keyFile = join(homedir(), '.tauri', 'steam-transfer.key');
  const env = { ...process.env };
  if (!env.TAURI_SIGNING_PRIVATE_KEY) {
    if (!existsSync(keyFile)) throw new Error(`Signing key not found at ${keyFile}`);
    env.TAURI_SIGNING_PRIVATE_KEY = readFileSync(keyFile, 'utf8');
  }
  env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ??= '';

  console.log(`\n== Bumping to ${version}`);
  run('node', ['scripts/bump.mjs', version]);
  console.log('\n== Testing');
  run('cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--lib', '--quiet']);
  console.log('\n== Building and signing');
  run('npx', ['tauri', 'build'], { env, shell: true });
  console.log('\n== Committing and tagging');
  run('git', ['commit', '-am', tag]);
  run('git', ['tag', '-a', tag, '-m', tag]);
}

// Make sure the commit and tag are on GitHub (harmless if they already are).
run('git', ['push', 'origin', 'HEAD']);
run('git', ['push', 'origin', tag]);

// Collect the installer + signature under names without spaces, and write the updater manifest.
const bundle = 'src-tauri/target/release/bundle/nsis';
const built = join(bundle, `Steam Transfer_${version}_x64-setup.exe`);
if (!existsSync(built) || !existsSync(`${built}.sig`)) throw new Error(`No signed build for ${version} at ${built}`);
const dist = 'src-tauri/target/release-upload';
rmSync(dist, { recursive: true, force: true });
mkdirSync(dist, { recursive: true });
const asset = `Steam-Transfer_${version}_x64-setup.exe`;
copyFileSync(built, join(dist, asset));
copyFileSync(`${built}.sig`, join(dist, `${asset}.sig`));
const signature = readFileSync(`${built}.sig`, 'utf8').trim();
const url = `https://github.com/${owner}/${repo}/releases/download/${tag}/${asset}`;
writeFileSync(join(dist, 'latest.json'), JSON.stringify({
  version,
  notes,
  pub_date: new Date().toISOString(),
  platforms: {
    'windows-x86_64': { signature, url },
    'windows-x86_64-nsis': { signature, url },
  },
}, null, 2));

console.log(`\n== Publishing ${tag} to ${owner}/${repo}`);
run('gh', ['release', 'create', tag,
  join(dist, asset), join(dist, `${asset}.sig`), join(dist, 'latest.json'),
  '--repo', `${owner}/${repo}`, '--title', `Steam Transfer ${tag}`, '--notes', notes, '--latest', '--verify-tag'],
{ env: ghEnv });
console.log(`\nDone: https://github.com/${owner}/${repo}/releases/tag/${tag}`);
