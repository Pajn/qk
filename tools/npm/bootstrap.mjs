#!/usr/bin/env node
// Generate metadata-only 0.0.0 packages for the initial local publication.
// node tools/npm/bootstrap.mjs --repository owner/repo --out /path/to/packages
import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  repository: { type: 'string' },
  out: { type: 'string' },
} });
if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(values.repository ?? '') || !values.out) {
  throw new Error('Use --repository owner/repo --out /path/to/packages');
}
const root = fileURLToPath(new URL('../../', import.meta.url));
const output = resolve(values.out);
const platforms = [
  ['linux', 'x64'], ['linux', 'arm64'], ['darwin', 'arm64'], ['win32', 'x64'],
];
const common = {
  version: '0.0.0',
  license: 'MIT OR Apache-2.0',
  repository: { type: 'git', url: `git+https://github.com/${values.repository}.git` },
  publishConfig: { access: 'public', tag: 'bootstrap', registry: 'https://registry.npmjs.org' },
};
function writePackage(name, fields) {
  const dir = join(output, name);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'package.json'), JSON.stringify({ name, ...common, ...fields }, null, 2) + '\n');
  writeFileSync(join(dir, 'README.md'), `# ${name}\n\nThis 0.0.0 package bootstraps trusted publishing for qk, the Rust task runner.\nIt contains no native binary and is not a functional CLI release.\nFuture releases are built by https://github.com/${values.repository}.\n`);
  for (const license of ['LICENSE-MIT', 'LICENSE-APACHE']) {
    copyFileSync(join(root, license), join(dir, license));
  }
  return dir;
}
const optionalDependencies = {};
for (const [os, cpu] of platforms) {
  const name = `@runqk/cli-${os}-${cpu}`;
  writePackage(name, {
    description: `Trusted publishing bootstrap for the qk ${os} ${cpu} binary`,
    os: [os], cpu: [cpu], files: ['LICENSE-MIT', 'LICENSE-APACHE'],
  });
  optionalDependencies[name] = common.version;
}
const main = writePackage('@runqk/cli', {
  description: 'Trusted publishing bootstrap for qk, a standalone Nx-compatible task runner',
  bin: { qk: 'bin/qk.js' }, files: ['bin', 'LICENSE-MIT', 'LICENSE-APACHE'], optionalDependencies,
});
mkdirSync(join(main, 'bin'), { recursive: true });
writeFileSync(join(main, 'bin', 'qk.js'), `#!/usr/bin/env node
console.error('@runqk/cli 0.0.0 only bootstraps trusted publishing; the qk executable is not included.');
process.exitCode = 1;
`, { mode: 0o755 });
console.log(`Generated five metadata-only bootstrap packages in ${output}`);
