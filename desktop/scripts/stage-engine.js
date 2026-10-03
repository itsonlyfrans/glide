'use strict';
// Builds the Rust engine (glided) in release mode and stages it where Tauri bundles it next to the app:
//   src-tauri/binaries/glided-<target triple>[.exe]
//   node scripts/stage-engine.js            build + stage
//   node scripts/stage-engine.js --no-build stage an already built binary
// Env: GLIDE_TARGET_DIR (cargo target dir; default core/target), GLIDE_CARGO_TARGET (cross target triple).
const { spawnSync, execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..', '..');
const core = path.join(root, 'core');
const targetDir = process.env.GLIDE_TARGET_DIR || path.join(core, 'target');
const triple = process.env.GLIDE_CARGO_TARGET
  || execFileSync('rustc', ['-vV'], { encoding: 'utf8' }).match(/^host: (.+)$/m)[1].trim();
const windows = triple.includes('windows');
const exe = windows ? 'glided.exe' : 'glided';

if (!process.argv.includes('--no-build')) {
  const args = ['build', '--release', '--locked', '-p', 'glide-daemon', '--bin', 'glided', '--target', triple];
  console.log(`cargo ${args.join(' ')}`);
  const r = spawnSync('cargo', args, { cwd: core, stdio: 'inherit', env: { ...process.env, CARGO_TARGET_DIR: targetDir } });
  if (r.status !== 0) { console.error('Engine build failed.'); process.exit(r.status ?? 1); }
}

const built = path.join(targetDir, triple, 'release', exe);
if (!fs.existsSync(built)) { console.error(`Engine binary not found: ${built}`); process.exit(1); }
const out = path.join(__dirname, '..', 'src-tauri', 'binaries');
fs.mkdirSync(out, { recursive: true });
const staged = path.join(out, `glided-${triple}${windows ? '.exe' : ''}`);
fs.copyFileSync(built, staged);
if (!windows) fs.chmodSync(staged, 0o755);
console.log(`Staged ${built} -> ${staged}`);
