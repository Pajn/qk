#!/usr/bin/env node
// Behavioral oracle for cache reuse, stdin selection and queued watch callbacks.
// Each scenario gets its own repository and cache; records live outside inputs.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, rmSync, existsSync, symlinkSync, utimesSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';

const here = dirname(fileURLToPath(import.meta.url));
const [mode, binaryArg, goldenArg] = process.argv.slice(2);
if (!['capture', 'compare'].includes(mode) || !binaryArg || !goldenArg) {
  throw new Error('usage: behavior.mjs capture|compare <nx|qk binary> <golden.json>');
}
const binary = resolve(binaryArg);
const scratch = mkdtempSync(join(process.platform === 'win32' ? tmpdir() : '/tmp', 'qkb-'));
const env = { ...process.env, NX_DAEMON: 'false', NX_NO_CLOUD: 'true', NX_TUI: 'false', NX_SKIP_NX_CACHE: 'false', NX_SKIP_REMOTE_CACHE: 'true' };
const result = {};
let sequence = 0;

function write(root, path, value) {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), typeof value === 'string' ? value : JSON.stringify(value));
}
function workspace() {
  const root = join(scratch, `workspace-${sequence++}`);
  mkdirSync(root);
  write(root, 'nx.json', { neverConnectToCloud: true });
  write(root, 'package.json', { name: 'behavior-fixture', private: true });
  write(root, '.gitignore', '.nx/\n.qk/\nnode_modules/\n**/dist/\n');
  const modules = join(here, 'nx/node_modules');
  if (existsSync(modules)) symlinkSync(modules, join(root, 'node_modules'), 'junction');
  const records = join(scratch, `records-${sequence}`);
  mkdirSync(records);
  for (const args of [['init', '-q'], ['add', '.'], ['-c', 'user.name=parity', '-c', 'user.email=parity@example.invalid', 'commit', '-qm', 'fixture']]) {
    const git = spawnSync('git', args, { cwd: root, encoding: 'utf8' });
    if (git.status !== 0) throw new Error(git.stderr);
  }
  return { root, records };
}
function run(root, args, extra = {}) {
  const executed = spawnSync(binary, args, { cwd: root, encoding: 'utf8', timeout: 60_000, env: { ...env, ...extra.env }, input: extra.input });
  if (executed.status !== 0) throw new Error(`${args.join(' ')} failed: ${executed.error ?? ''}\n${executed.stdout}\n${executed.stderr}`);
  return executed.stdout;
}
function tasks(records) { return readdirSync(records).filter(name => name.endsWith('.ran')).sort(); }
function clear(records) { for (const name of readdirSync(records)) rmSync(join(records, name)); }

const taskScript = `import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
const task = process.argv[2];
const records = process.env.BEHAVIOR_RECORDS;
writeFileSync(join(records, task + '.ran'), task);
if (task === 'producer') {
  const input = readFileSync('producer/source.txt', 'utf8');
  mkdirSync('producer/dist/nested/empty', {recursive:true});
  writeFileSync('producer/dist/value.d.ts', input.split('\\n')[0]);
  writeFileSync('producer/dist/value.js', input);
} else {
  mkdirSync('app/dist', {recursive:true});
  writeFileSync('app/dist/result', readFileSync('producer/dist/value.d.ts'));
}`;
try {
  {
    const { root, records } = workspace();
    write(root, 'record.mjs', `import {writeFileSync} from 'node:fs';import {join} from 'node:path';writeFileSync(join(process.env.BEHAVIOR_RECORDS,process.argv[2]+'.json'),JSON.stringify({configuration:process.env.NX_TASK_TARGET_CONFIGURATION??null,base:process.env.QK_PARITY_BASE,both:process.env.QK_PARITY_BOTH,configured:process.env.QK_PARITY_CONFIG}));`);
    const target = (name, extra = {}) => ({ command: `node record.mjs ${name}`, cache: false, ...extra });
    write(root, 'app/project.json', { name: 'app', targets: { build: target('app', {
      defaultConfiguration: 'prod', dependsOn: ['lib:build', 'tool:build'],
      options: { env: { QK_PARITY_BASE: 'base', QK_PARITY_BOTH: 'base' } },
      configurations: { prod: { env: { QK_PARITY_CONFIG: 'prod', QK_PARITY_BOTH: 'prod' } }, empty: { env: {} } },
    }) } });
    write(root, 'lib/project.json', { name: 'lib', targets: { build: target('lib', { configurations: { prod: {} } }) } });
    write(root, 'tool/project.json', { name: 'tool', targets: { build: target('tool', {
      defaultConfiguration: 'dev', dependsOn: ['leaf:build'], configurations: { dev: {} },
    }) } });
    write(root, 'leaf/project.json', { name: 'leaf', targets: { build: target('leaf', { configurations: { prod: {}, dev: {} } }) } });
    for (const [name, project, args] of [
      ['default', 'app', []], ['explicit', 'app', ['-c', 'prod']],
      ['empty-env', 'app', ['-c', 'empty']], ['root-fallback', 'tool', ['-c', 'prod']],
    ]) {
      clear(records);
      run(root, ['run-many', '-t', 'build', '-p', project, ...args], { env: {
        BEHAVIOR_RECORDS: records, QK_PARITY_BASE: 'inherited-base',
        QK_PARITY_BOTH: 'inherited-both', QK_PARITY_CONFIG: 'inherited-config',
      } });
      result[`configuration-${name}`] = Object.fromEntries(readdirSync(records).sort().map(file => [
        file.replace(/\.json$/, ''), JSON.parse(readFileSync(join(records, file), 'utf8')),
      ]));
    }
  }
  for (const [name, indirect, transitive, pattern] of [
    ['direct', false, false, '**/*.d.ts'], ['indirect', true, false, '**/*.d.ts'],
    ['transitive', true, true, '**/*.d.ts'], ['broad-glob', false, false, '**/*'],
  ]) {
    const { root, records } = workspace();
    write(root, 'task.mjs', taskScript);
    write(root, 'producer/source.txt', 'declaration\nimplementation one');
    write(root, 'producer/project.json', { name: 'producer', targets: { build: { command: 'node task.mjs producer', cache: true, inputs: ['{projectRoot}/source.txt'], outputs: ['{projectRoot}/dist'] } } });
    write(root, 'middle/project.json', { name: 'middle', targets: { build: { executor: 'nx:noop', cache: true, inputs: [], outputs: [], dependsOn: [{ projects: ['producer'], target: 'build' }] } } });
    write(root, 'app/project.json', { name: 'app', targets: { build: { command: 'node task.mjs consumer', cache: true, inputs: [{ dependentTasksOutputFiles: pattern, transitive }], outputs: ['{projectRoot}/dist'], dependsOn: [{ projects: [indirect ? 'middle' : 'producer'], target: 'build' }] } } });
    const phases = [];
    let phase = 0;
    for (const input of ['declaration\nimplementation one', 'declaration\nimplementation one', 'declaration\nimplementation two', 'new declaration\nimplementation two']) {
      write(root, 'producer/source.txt', input);
      // Nx metadata caches may use coarse timestamps: make each edit distinct.
      const timestamp = new Date(Date.now() + (++phase * 2000));
      utimesSync(join(root, 'producer/source.txt'), timestamp, timestamp);
      clear(records);
      run(root, ['run', 'app:build'], { env: { BEHAVIOR_RECORDS: records } });
      phases.push(tasks(records));
    }
    result[`cache-${name}`] = phases;
  }
  {
    const { root, records } = workspace();
    write(root, '.nxignore', 'app/ignored/**\n!app/ignored/keep.txt\n');
    write(root, 'app/ignored/skip.txt', 'one');
    write(root, 'app/ignored/keep.txt', 'one');
    write(root, 'app/input with spaces.txt', 'one');
    write(root, 'app/project.json', { name: 'app', targets: { build: { command: 'node record.mjs', cache: true, inputs: ['{projectRoot}/**/*'], outputs: ['{projectRoot}/dist'] } } });
    write(root, 'record.mjs', `import {writeFileSync,mkdirSync} from 'node:fs'; import {join} from 'node:path'; writeFileSync(join(process.env.BEHAVIOR_RECORDS,'consumer.ran'),'ran'); mkdirSync('app/dist',{recursive:true});writeFileSync('app/dist/out','result');`);
    const phases = [];
    let phase = 0;
    for (const change of [null, ['app/ignored/skip.txt', 'two'], ['app/ignored/keep.txt', 'two']]) {
      if (change) {
        write(root, ...change);
        const timestamp = new Date(Date.now() + (++phase * 2000));
        utimesSync(join(root, change[0]), timestamp, timestamp);
      }
      clear(records);
      run(root, ['run', 'app:build'], { env: { BEHAVIOR_RECORDS: records } });
      phases.push(tasks(records));
    }
    result['cache-nxignore'] = phases;
    result['stdin-spaces'] = JSON.parse(run(root, ['show', 'projects', '--affected', '--stdin', '--json'], { input: 'app/input with spaces.txt\r\n\r\n' }).trim().split('\n').pop()).sort();
    result['stdin-empty'] = JSON.parse(run(root, ['show', 'projects', '--affected', '--stdin', '--json'], { input: '' }).trim().split('\n').pop()).sort();
  }
  for (const [name, ignoredParent] of [['excluded-parent', true], ['git-negation', false]]) {
    const { root, records } = workspace();
    const input = ignoredParent ? 'app/ignored/keep.txt' : 'app/keep.txt';
    if (!ignoredParent) write(root, '.gitignore', '.nx/\n.qk/\nnode_modules/\n**/dist/\napp/*.txt\n');
    write(root, '.nxignore', ignoredParent ? 'app/ignored/\n!app/ignored/keep.txt\n' : '!app/keep.txt\n');
    write(root, input, 'one');
    if (ignoredParent) {
      const tracked = spawnSync('git', ['add', input], { cwd: root, encoding: 'utf8' });
      if (tracked.status !== 0) throw new Error(tracked.stderr);
    }
    write(root, 'app/project.json', { name: 'app', targets: { build: { command: 'node record.mjs', cache: true, inputs: ['{projectRoot}/**/*'], outputs: ['{projectRoot}/dist'] } } });
    write(root, 'record.mjs', `import {writeFileSync,mkdirSync} from 'node:fs';import {join} from 'node:path';writeFileSync(join(process.env.BEHAVIOR_RECORDS,'consumer.ran'),'ran');mkdirSync('app/dist',{recursive:true});writeFileSync('app/dist/out','result');`);
    const phases = [];
    for (const [phase, content] of ['one', 'two'].entries()) {
      write(root, input, content);
      const timestamp = new Date(Date.now() + (phase + 1) * 2000);
      utimesSync(join(root, input), timestamp, timestamp);
      clear(records);
      run(root, ['run', 'app:build'], { env: { BEHAVIOR_RECORDS: records } });
      phases.push(tasks(records));
    }
    result[`cache-${name}`] = phases;
    result[`affected-${name}`] = JSON.parse(run(root, ['show', 'projects', '--affected', '--stdin', '--json'], { input: input + '\n' }).trim().split('\n').pop()).sort();
  }
  {
    const { root, records } = workspace();
    write(root, 'app/project.json', { name: 'app' });
    write(root, 'app/first.txt', 'one');
    write(root, 'app/second.txt', 'one');
    write(root, 'callback.mjs', `import {writeFileSync,existsSync,readdirSync,renameSync} from 'node:fs';import {join} from 'node:path';import {setTimeout as delay} from 'node:timers/promises';const root=process.env.BEHAVIOR_RECORDS;const n=readdirSync(root).filter(x=>x.endsWith('.json')).length;writeFileSync(join(root,n+'.tmp'),JSON.stringify({project:process.env.NX_PROJECT_NAME,files:(process.env.NX_FILE_CHANGES||'').split(' ').filter(Boolean).sort()}));renameSync(join(root,n+'.tmp'),join(root,n+'.json'));if(n===1){while(!existsSync(join(root,'release')))await delay(20);}`);
    // Let filesystem creation events settle before either watcher subscribes.
    await delay(1100);
    const isNx = mode === 'capture';
    const watchEnv = { ...env, NX_DAEMON: isNx ? 'true' : 'false', BEHAVIOR_RECORDS: records, NX_SOCKET_DIR: join(scratch, 'socket'), NX_DAEMON_SOCKET_DIR: join(scratch, 'daemon') };
    // Mentioning NX_PROJECT_NAME requests one callback per changed project.
    const child = spawn(binary, ['watch', '-p', 'app', '--initialRun', '--verbose', '--', 'node callback.mjs NX_PROJECT_NAME'], { cwd: root, env: watchEnv, detached: process.platform !== 'win32', stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    child.stdout.on('data', chunk => output += chunk);
    child.stderr.on('data', chunk => output += chunk);
    const closed = new Promise(resolve => child.once('close', resolve));
    let exited = false;
    child.on('exit', () => exited = true);
    const wait = async (predicate) => {
      const deadline = Date.now() + 30_000;
      while (!predicate()) {
        if (exited || Date.now() > deadline) throw new Error(`watch failed/timed out:\n${output}`);
        await delay(20);
      }
    };
    try {
      await wait(() => output.includes(isNx ? 'watch process waiting' : 'qk: watching'));
      write(root, 'app/first.txt', 'two');
      await wait(() => existsSync(join(records, '1.json')));
      write(root, 'app/second.txt', 'two');
      // Give both backends time to queue the edit while callback 1 is blocked.
      await delay(500);
      writeFileSync(join(records, 'release'), 'release');
      await wait(() => existsSync(join(records, '2.json')));
      const callbacks = readdirSync(records).filter(name => name.endsWith('.json')).sort().map(name => JSON.parse(readFileSync(join(records, name), 'utf8')));
      result.watch = { initial: callbacks[0], first: callbacks[1], queued: callbacks.slice(2) };
    } finally {
      if (process.platform !== 'win32') { try { process.kill(-child.pid, 'SIGINT'); } catch {} }
      else child.kill();
      await Promise.race([closed, delay(3000)]);
      if (!exited && process.platform !== 'win32') {
        try { process.kill(-child.pid, 'SIGKILL'); } catch {}
        await Promise.race([closed, delay(3000)]);
      }
      if (isNx) spawnSync(binary, ['reset', '--onlyDaemon'], { cwd: root, env: watchEnv, timeout: 10_000 });
    }
  }
  const text = JSON.stringify(result, null, 2) + '\n';
  if (mode === 'capture') writeFileSync(goldenArg, text);
  else if (text !== readFileSync(goldenArg, 'utf8')) throw new Error(`behavior differs from Nx golden:\n${text}`);
  console.log(`ok   behavioral ${mode}: configurations, cache, ignore rules, stdin and watch`);
} finally { rmSync(scratch, { recursive: true, force: true }); }
