'use strict';
// Sets Glide's version everywhere it is written, so the window, the engine and the update files always agree.
//   node scripts/set-version.js 0.2.0
// Then refresh the lock files (cargo update --workspace in core/ and desktop/src-tauri/) and commit.
const fs = require('node:fs');
const path = require('node:path');

const version = process.argv[2];
if (!/^\d+\.\d+\.\d+$/.test(version || '')) {
  console.error('Usage: node scripts/set-version.js X.Y.Z');
  process.exit(1);
}
const root = path.resolve(__dirname, '..');
const edits = [
  ['core/Cargo.toml', /(\[workspace\.package\][^[]*?\nversion = ")[^"]+(")/],
  ['desktop/src-tauri/Cargo.toml', /(\[package\][^[]*?\nversion = ")[^"]+(")/],
  ['desktop/src-tauri/tauri.conf.json', /("version": ")[^"]+(")/],
  ['desktop/package.json', /("version": ")[^"]+(")/],
];
for (const [file, pattern] of edits) {
  const full = path.join(root, file);
  const text = fs.readFileSync(full, 'utf8');
  if (!pattern.test(text)) { console.error(`No version found in ${file}`); process.exit(1); }
  fs.writeFileSync(full, text.replace(pattern, `$1${version}$2`));
  console.log(`${file} -> ${version}`);
}
