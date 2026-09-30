#!/usr/bin/env node
// Exercise packed npm installation and the release binary outside the checkout.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: { packages: { type: 'string' }, binary: { type: 'string' } } });
if (!values.packages || !values.binary) throw new Error('usage: smoke.mjs --packages <generated npm directory> --binary <release binary>');
const packages = resolve(values.packages);
const binary = resolve(values.binary);
const scratch = mkdtempSync(join(tmpdir(), 'qk install smoke '));
const env = { ...process.env, npm_config_cache: join(scratch, 'npm-cache'), npm_config_update_notifier: 'false' };
function run(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, env, encoding: 'utf8', timeout: 60_000 });
  if (result.status !== 0) throw new Error(`${command} ${args.join(' ')} failed: ${result.error ?? ''}\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}
function npm(args, cwd) {
  // npm's Windows entry point is a cmd shim. Quote paths, including our
  // deliberate spaces, rather than relying on implicit shell interpolation.
  return process.platform === 'win32'
    ? run('cmd.exe', ['/d', '/s', '/c', `npm ${args.map(arg => '"' + arg.replaceAll('"', '""') + '"').join(' ')}`], cwd)
    : run('npm', args, cwd);
}
function check(label, invoke) {
  const root = join(scratch, label);
  mkdirSync(join(root, 'app/src'), { recursive: true });
  const write = (name, content) => writeFileSync(join(root, name), typeof content === 'string' ? content : JSON.stringify(content));
  write('nx.json', {});
  write('package.json', { name: 'installed-smoke', private: true });
  write('.gitignore', '.qk/\napp/dist/\n');
  write('app/project.json', { name: 'app', targets: { build: {
    command: 'node build.mjs', cache: true,
    inputs: ['{projectRoot}/src/**/*'], outputs: ['{projectRoot}/dist'],
  } } });
  write('app/src/input.txt', 'one');
  write('build.mjs', `import {appendFileSync,mkdirSync,readFileSync,writeFileSync} from 'node:fs';
appendFileSync('executions.txt','ran\\n');mkdirSync('app/dist',{recursive:true});
writeFileSync('app/dist/result.txt',readFileSync('app/src/input.txt'));\n`);
  assert.match(invoke(['--version'], root), /^qk \d+\.\d+\.\d+/);
  assert.deepEqual(JSON.parse(invoke(['show', 'projects', '--json'], root)), ['app']);
  const doctor = JSON.parse(invoke(['doctor', '--json', '--strict'], root));
  assert.equal(doctor.errors, 0);
  assert.equal(doctor.warnings, 0);
  invoke(['run', 'app:build', '--output-style', 'static'], root);
  assert.equal(readFileSync(join(root, 'app/dist/result.txt'), 'utf8'), 'one');
  rmSync(join(root, 'app/dist'), { recursive: true });
  invoke(['run', 'app:build', '--output-style', 'static'], root);
  assert.equal(readFileSync(join(root, 'executions.txt'), 'utf8'), 'ran\n');
  assert.equal(readFileSync(join(root, 'app/dist/result.txt'), 'utf8'), 'one');
  write('app/src/input.txt', 'changed input');
  invoke(['run', 'app:build', '--output-style', 'static'], root);
  assert.equal(readFileSync(join(root, 'executions.txt'), 'utf8'), 'ran\nran\n');
  assert.equal(readFileSync(join(root, 'app/dist/result.txt'), 'utf8'), 'changed input');
  console.log(`ok: ${label} version, discovery, doctor, execution, cache restore and invalidation`);
}
try {
  // Pack the actual generated metadata, wrapper and native platform package.
  const main = join(packages, '@runqk/cli');
  const platform = join(packages, `@runqk/cli-${process.platform}-${process.arch}`);
  if (!existsSync(platform)) throw new Error(`missing host platform package: ${platform}`);
  const packed = [platform, main].map(path => {
    const result = JSON.parse(npm(['pack', path, '--json', '--ignore-scripts', '--pack-destination', scratch], scratch));
    return join(scratch, result[0].filename);
  });
  const install = join(scratch, 'installation');
  mkdirSync(install);
  writeFileSync(join(install, 'package.json'), '{"private":true}');
  npm(['install', '--offline', '--ignore-scripts', '--no-audit', '--no-fund', ...packed], install);
  const installedPackage = JSON.parse(readFileSync(join(install, 'node_modules/@runqk/cli/package.json')));
  assert.equal(installedPackage.name, '@runqk/cli');
  // Invoke the installed npm command shim with PATH, from a separate workspace.
  const bin = join(install, 'node_modules/.bin');
  const pathKey = Object.keys(env).find(key => key.toLowerCase() === 'path') ?? 'PATH';
  env[pathKey] = bin + (process.platform === 'win32' ? ';' : ':') + (env[pathKey] ?? '');
  check('npm-installed', (args, cwd) => process.platform === 'win32'
    ? run('cmd.exe', ['/d', '/s', '/c', `qk ${args.map(arg => '"' + arg + '"').join(' ')}`], cwd)
    : run('qk', args, cwd));

  const archiveRoot = join(scratch, 'archive');
  const extracted = join(scratch, 'extracted');
  mkdirSync(archiveRoot);
  mkdirSync(extracted);
  copyFileSync(binary, join(archiveRoot, basename(binary)));
  const archive = join(scratch, 'qk.tar.gz');
  run('tar', ['-czf', archive, '-C', archiveRoot, basename(binary)], scratch);
  run('tar', ['-xzf', archive, '-C', extracted], scratch);
  check('archive-extracted', (args, cwd) => run(join(extracted, basename(binary)), args, cwd));
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
