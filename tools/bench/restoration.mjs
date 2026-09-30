// A restoration matrix sharing the main benchmark's graph and source fixture.
// Preparation and content/cache-hit verification run outside every timing window.
import { spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

export function measureRestoration({ root, qk, projects, runs, filesPerProject }) {
  mkdirSync(join(root, ".qk"), {recursive:true});
  const base = readFileSync(join(root, 'nx.json'), 'utf8');
  writeFileSync(join(root, '.qk/restore-base.json'), base);
  writeFileSync(join(root, 'restore-bench.mjs'), `
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
const [mode, content, state, selection] = process.argv.slice(2);
const root = mode === 'build' ? resolve('../..') : process.cwd();
const selected = mode === 'build' ? state : selection;
const count = ${filesPerProject};
const bytes = (project, file, ext) => content === 'repeated' ? 'x'.repeat(1500) : (project + ':' + file + ':' + ext + ':').padEnd(1500, 'x');
const info = project => content === 'repeated' ? '{}' : JSON.stringify({project});
const projects = () => readdirSync('packages').sort();
if (mode === 'build') {
  const project = basename(process.cwd());
  const counter = join(root, '.qk/restore-builds', project);
  mkdirSync(join(root, '.qk/restore-builds'), {recursive:true});
  writeFileSync(counter, String((existsSync(counter) ? Number(readFileSync(counter, 'utf8')) : 0) + 1));
  mkdirSync('dist', {recursive:true});
  for(let file=0; file<count; file++) for(const ext of ['js','d.ts','js.map']) writeFileSync('dist/file'+file+'.'+ext, bytes(project,file,ext));
  writeFileSync('dist/tsconfig.tsbuildinfo', info(project));
  if(selected === 'excluded') { mkdirSync('dist/keep', {recursive:true}); writeFileSync('dist/keep/sentinel', 'unselected'); }
} else if (mode === 'prepare') {
  const config = JSON.parse(readFileSync('.qk/restore-base.json','utf8'));
  config.targetDefaults.build.command = 'node ../../restore-bench.mjs build '+content+' '+selection;
  config.targetDefaults.build.outputs = selection === 'excluded' ? ['{projectRoot}/dist','!{projectRoot}/dist/keep'] : ['{projectRoot}/dist'];
  writeFileSync('nx.json', JSON.stringify(config));
  rmSync('.nxignore', {force:true});
  for(const project of projects()) {
    const dist = join('packages',project,'dist');
    if(state === 'absent') rmSync(dist,{recursive:true,force:true});
    else if(state === 'stale') { mkdirSync(dist,{recursive:true}); writeFileSync(join(dist,'file0.js'),'stale'); writeFileSync(join(dist,'obsolete'),'stale'); }
    else {
      const sentinel = join(dist,'keep/sentinel');
      if(existsSync(sentinel) && readFileSync(sentinel,'utf8') !== 'unselected') throw new Error('excluded file was changed');
      mkdirSync(join(dist,'keep'),{recursive:true});
      if(!existsSync(sentinel)) writeFileSync(sentinel,'unselected');
      for(const entry of readdirSync(dist)) if(entry !== 'keep') rmSync(join(dist,entry),{recursive:true,force:true});
    }
  }
} else if (mode === 'verify') {
  const counts = JSON.parse(readFileSync('.qk/restore-counts.json','utf8'));
  for(const project of projects()) {
    if(Number(readFileSync(join('.qk/restore-builds',project),'utf8')) !== counts[project]) throw new Error('restoration benchmark executed '+project);
    const dist = join('packages',project,'dist');
    for(let file=0; file<count; file++) for(const ext of ['js','d.ts','js.map']) {
      if(readFileSync(join(dist,'file'+file+'.'+ext),'utf8') !== bytes(project,file,ext)) throw new Error('incorrect restored output');
    }
    if(readFileSync(join(dist,'tsconfig.tsbuildinfo'),'utf8') !== info(project)) throw new Error('incorrect build info');
    if(existsSync(join(dist,'obsolete'))) throw new Error('stale output survived restoration');
    if(selection === 'excluded' && readFileSync(join(dist,'keep/sentinel'),'utf8') !== 'unselected') throw new Error('excluded file changed');
  }
} else throw new Error('unknown restoration benchmark mode');
`);
  const run = (program, args) => {
    const result = spawnSync(program, args, { cwd: root, encoding: 'utf8' });
    if (result.error || result.status !== 0) throw new Error(`${program} failed: ${result.error ?? result.stderr}`);
    return result;
  };
  const matrix = ['repeated', 'distinct'].flatMap(content => [
    { content, state: 'absent', selection: 'complete' },
    { content, state: 'stale', selection: 'complete' },
    { content, state: 'partial', selection: 'excluded' },
  ]);
  try {
    for (const content of ['repeated', 'distinct']) for (const selection of ['complete', 'excluded']) {
      run(process.execPath, ['restore-bench.mjs', 'prepare', content, selection === 'complete' ? 'absent' : 'partial', selection]);
      run(qk, ['run-many', '-t', 'build', '--output-style', 'static']);
    }
    const counts = Object.fromEntries(Array.from({length:projects}, (_, index) => {
      const project = `p${String(index).padStart(4,'0')}`;
      return [project, Number(readFileSync(join(root,'.qk/restore-builds',project),'utf8'))];
    }));
    writeFileSync(join(root,'.qk/restore-counts.json'), JSON.stringify(counts));
    const results = join(root, 'restore-hyperfine.json');
    // Quote the absolute binary path for the POSIX shell used by hyperfine.
    const quoted = `'${qk.replaceAll("'", "'\\''")}'`;
    const measured = spawnSync('hyperfine', [
      '--warmup', '2', '--runs', String(runs), '--export-json', results, '--output', 'null',
      ...matrix.flatMap(({content,state,selection}) => [
        '--prepare', `node restore-bench.mjs prepare ${content} ${state} ${selection}`,
        '--conclude', `node restore-bench.mjs verify ${content} ${state} ${selection}`,
        '--command-name', `restore, ${content} blobs, ${state} outputs, ${selection}`,
        `${quoted} run-many -t build --output-style static`,
      ]),
    ], { cwd: root, stdio: ['ignore','inherit','inherit'] });
    if (measured.error || measured.status !== 0) throw new Error(`restoration benchmark failed: ${measured.error ?? measured.status}`);
    return JSON.parse(readFileSync(results, 'utf8')).results;
  } finally {
    writeFileSync(join(root, 'nx.json'), base);
  }
}
