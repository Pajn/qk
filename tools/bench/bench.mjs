#!/usr/bin/env node
// Benchmarks qk's hot paths on a generated workspace with hyperfine.
//
//   node tools/bench/bench.mjs --qk target/release/qk [--projects 300] [--json out.json] [--markdown out.md]
//
// The workspace is generated deterministically in a scratch directory: a
// layered dependency graph of packages, each with source files, a build target
// that is cached, and a pnpm lockfile installing a pool of external packages
// with their own dependencies, about as large as a sizeable monorepo's. Each
// command is measured after a warm-up, with the cache populated first:
//
// - show projects: configuration and graph loading
// - graph: the full project graph as JSON
// - affected, no changes: change detection with a base equal to the head
// - run-many, all cached: hashing every task, with every output kept in place
// - run-many, restoring outputs: the same with every output restored from the
//   cache, as after a checkout or a clean
//
// Needs hyperfine and git on PATH.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { setTimeout as delay } from "node:timers/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index >= 0 ? args[index + 1] : fallback;
};
const qk = resolve(option("--qk", "target/release/qk"));
const projects = Number(option("--projects", "300"));
const runs = Number(option("--runs", "10"));
if (!Number.isInteger(projects) || projects < 1 || !Number.isInteger(runs) || runs < 1) {
  throw new Error('--projects and --runs must be positive integers');
}
const externals = projects * 10;
const filesPerProject = 10;
// Each build writes a JavaScript file, a declaration and a source map per
// source file, and a build info file, as tsc does.
const outputsPerProject = filesPerProject * 3 + 1;

// A small deterministic generator, so every run benchmarks the same workspace.
let seed = 42;
const random = (limit) => {
  seed = (seed * 1103515245 + 12345) % 2 ** 31;
  return seed % limit;
};

const root = mkdtempSync(join(tmpdir(), "qk-bench-"));
process.on("exit", () => rmSync(root, { recursive: true, force: true }));
const write = (path, text) => {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), text);
};

const name = (index) => `p${String(index).padStart(4, "0")}`;
const external = (index) => `ext-${String(index).padStart(5, "0")}`;

write(
  "nx.json",
  JSON.stringify({
    namedInputs: {
      default: ["{projectRoot}/**/*"],
      production: ["default", "!{projectRoot}/**/*.test.ts"],
    },
    targetDefaults: {
      build: {
        cache: true,
        command: "node ../../build.mjs",
        options: { cwd: "{projectRoot}" },
        dependsOn: ["^build"],
        inputs: ["production", "^production"],
        outputs: ["{projectRoot}/dist"],
      },
    },
  }),
);
write(
  "build.mjs",
  [
    'import { mkdirSync, writeFileSync } from "node:fs";',
    'mkdirSync("dist", { recursive: true });',
    `for (let file = 0; file < ${filesPerProject}; file++) {`,
    '  for (const extension of ["js", "d.ts", "js.map"]) writeFileSync(`dist/file${file}.${extension}`, "x".repeat(1500));',
    "}",
    'writeFileSync("dist/tsconfig.tsbuildinfo", "{}");',
    "",
  ].join("\n"),
);
write(
  "clean.mjs",
  [
    'import { readdirSync, rmSync } from "node:fs";',
    'for (const project of readdirSync("packages")) rmSync(`packages/${project}/dist`, { recursive: true, force: true });',
    "",
  ].join("\n"),
);
write("package.json", JSON.stringify({ name: "bench", private: true }));
write("pnpm-workspace.yaml", "packages:\n  - packages/*\n");
write(".gitignore", "node_modules/\ndist/\n.qk/\n");
write("prepare.mjs", `import {writeFileSync,rmSync} from 'node:fs';
if(process.argv.includes('nxignore'))writeFileSync('.nxignore','**/ignored/**\\n');
else rmSync('.nxignore',{force:true});
if(process.argv.includes('restore'))await import('./clean.mjs');\n`);
write("callback.mjs", `import {writeFileSync} from 'node:fs';
if((process.env.NX_FILE_CHANGES||'').split(' ').includes(process.env.BENCH_INPUT))
writeFileSync(process.env.BENCH_RECORD,String(Date.now()));\n`);

const lock = ["lockfileVersion: '9.0'", "", "importers:", "", "  .: {}", ""];
for (let index = 0; index < projects; index++) {
  // Each project depends on up to three earlier ones, so the graph is layered.
  const workspace = [...new Set(Array.from({ length: Math.min(index, 3) }, () => name(random(index))))];
  const installs = [...new Set(Array.from({ length: 20 }, () => external(random(externals))))].sort();
  const dependencies = Object.fromEntries([
    ...workspace.map((dependency) => [`@bench/${dependency}`, "workspace:*"]),
    ...installs.map((dependency) => [dependency, "1.0.0"]),
  ]);
  write(
    `packages/${name(index)}/package.json`,
    JSON.stringify({ name: `@bench/${name(index)}`, version: "1.0.0", dependencies, nx: { targets: { build: {} } } }),
  );
  for (let file = 0; file < filesPerProject; file++) {
    write(`packages/${name(index)}/src/file${file}.ts`, `export const value${file} = ${index * 100 + file};\n`);
  }
  write(`packages/${name(index)}/src/file0.test.ts`, "export {};\n");
  write(`packages/${name(index)}/ignored/generated.txt`, "ignored by .nxignore\n");
  lock.push(`  packages/${name(index)}:`, "    dependencies:");
  for (const dependency of workspace) {
    lock.push(`      '@bench/${dependency}':`, "        specifier: workspace:*", `        version: link:../${dependency}`);
  }
  for (const dependency of installs) {
    lock.push(`      ${dependency}:`, "        specifier: 1.0.0", "        version: 1.0.0");
  }
  lock.push("");
}
lock.push("packages:", "");
for (let index = 0; index < externals; index++) {
  lock.push(`  ${external(index)}@1.0.0:`, `    resolution: {integrity: sha512-${external(index)}}`, "");
}
lock.push("snapshots:", "");
for (let index = 0; index < externals; index++) {
  const dependencies = index === 0 ? [] : [...new Set(Array.from({ length: random(4) }, () => external(random(index))))].sort();
  if (dependencies.length === 0) {
    lock.push(`  ${external(index)}@1.0.0: {}`, "");
    continue;
  }
  lock.push(`  ${external(index)}@1.0.0:`, "    dependencies:");
  for (const dependency of dependencies) lock.push(`      ${dependency}: 1.0.0`);
  lock.push("");
}
write("pnpm-lock.yaml", `${lock.join("\n")}\n`);

const git = (...command) => {
  const result = spawnSync("git", command, {
    cwd: root,
    env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" },
  });
  if (result.status !== 0) throw new Error(`git ${command.join(" ")}: ${result.stderr}`);
};
git("init", "--quiet", "--initial-branch=main");
git("add", "--all");
git("-c", "user.name=bench", "-c", "user.email=bench@example.invalid", "commit", "--quiet", "-m", "bench");

// Populate the cache, so the run-many benchmark measures a fully cached run.
const warm = spawnSync(qk, ["run-many", "-t", "build", "--output-style", "static"], { cwd: root, encoding: "utf8" });
if (warm.status !== 0) throw new Error(`warm-up run failed:\n${warm.stderr}`);

const commands = [
  ["show projects", `${qk} show projects --json`],
  ["graph", `${qk} graph --file -`],
  ["affected, no changes", `${qk} show projects --affected --base HEAD --head HEAD`],
  ["run-many, all cached", `${qk} run-many -t build --output-style static`],
];
// Commands measured with every output removed before each run.
const restoring = [["run-many, restoring outputs", `${qk} run-many -t build --output-style static`]];
const nxignore = [
  ["run-many, .nxignore, all cached", "node prepare.mjs nxignore"],
  ["run-many, .nxignore, restoring outputs", "node prepare.mjs nxignore restore"],
];
// Populate both input-policy cache keys before timed runs.
write(".nxignore", "**/ignored/**\n");
const ignoredWarm = spawnSync(qk, ["run-many", "-t", "build", "--output-style", "static"], { cwd: root, encoding: "utf8" });
if (ignoredWarm.status !== 0) throw new Error(`.nxignore warm-up failed:\n${ignoredWarm.stderr}`);
rmSync(join(root, ".nxignore"));
const results = join(root, "hyperfine.json");
const hyperfine = spawnSync(
  "hyperfine",
  [
    "--warmup", "2",
    "--runs", String(runs),
    "--export-json", results,
    "--output", "null",
    ...commands.flatMap(([label, command]) => ["--prepare", "node prepare.mjs", "--command-name", label, command]),
    ...restoring.flatMap(([label, command]) => ["--prepare", "node prepare.mjs restore", "--command-name", label, command]),
    ...nxignore.flatMap(([label, prepare]) => ["--prepare", prepare, "--command-name", label, `${qk} run-many -t build --output-style static`]),
  ],
  { cwd: root, stdio: ["ignore", "inherit", "inherit"] },
);
if (hyperfine.status !== 0) process.exit(hyperfine.status ?? 1);

const { results: measured } = JSON.parse(readFileSync(results, "utf8"));
// Callback timestamps exclude the polling delay used to collect each sample.
// This includes native event delivery, debounce, graph reload and Node startup.
for (const policy of ["git", "nxignore"]) {
  if (policy === "nxignore") write(".nxignore", "**/ignored/**\n");
  else rmSync(join(root, ".nxignore"), { force: true });
  const record = join(root, `.qk/watch-${policy}.txt`);
  mkdirSync(dirname(record), { recursive: true });
  const input = `packages/${name(0)}/src/file0.ts`;
  const child = spawn(qk, ["watch", "-p", `@bench/${name(0)}`, "--", "node callback.mjs"], {
    cwd: root, detached: true, stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, BENCH_INPUT: input, BENCH_RECORD: record },
  });
  let output = '';
  let ended = false;
  let failure;
  const closed = new Promise(resolve => child.once('close', resolve));
  child.on('error', error => { failure = error; ended = true; });
  child.on('exit', () => ended = true);
  child.stdout.on('data', chunk => output += chunk);
  child.stderr.on('data', chunk => output += chunk);
  const wait = async predicate => {
    const deadline = Date.now() + 30_000;
    while (!predicate()) {
      if (ended || Date.now() > deadline) throw new Error(`watch benchmark failed: ${failure ?? output}`);
      await delay(10);
    }
  };
  const samples = [];
  try {
    await wait(() => output.includes('qk: watching'));
    for (let sample = 0; sample < runs + 2; sample++) {
      // Allow the previous callback to exit before the next edit.
      await delay(100);
      rmSync(record, { force: true });
      const start = Date.now();
      write(input, `export const value0 = ${sample};\n`);
      await wait(() => existsSync(record) && readFileSync(record, 'utf8').length > 0);
      if (sample >= 2) samples.push((Number(readFileSync(record, 'utf8')) - start) / 1000);
    }
  } finally {
    const signal = value => {
      try { process.kill(-child.pid, value); }
      catch { try { child.kill(value); } catch {} }
    };
    if (child.pid) signal('SIGINT');
    await Promise.race([closed, delay(3000)]);
    if (!ended && child.pid) {
      signal('SIGKILL');
      await Promise.race([closed, delay(3000)]);
    }
    if (!ended) {
      // A failed termination must not keep Node alive through child handles.
      child.unref();
      child.stdout.destroy();
      child.stderr.destroy();
      throw new Error('watch benchmark could not stop its child process');
    }
  }
  const mean = samples.reduce((a, b) => a + b, 0) / samples.length;
  measured.push({ command: `watch edit-to-callback, ${policy}`, mean,
    stddev: Math.sqrt(samples.reduce((sum, value) => sum + (value - mean) ** 2, 0) / samples.length),
    min: Math.min(...samples), max: Math.max(...samples) });
}
const summary = {
  projects,
  files: projects * (filesPerProject + 2),
  outputs: projects * outputsPerProject,
  externals,
  commands: Object.fromEntries(
    measured.map((result) => [result.command, { mean: result.mean, stddev: result.stddev, min: result.min, max: result.max }]),
  ),
};
const milliseconds = (seconds) => `${(seconds * 1000).toFixed(1)} ms`;
const markdown = [
  `qk on ${projects} projects, ${summary.files} files, ${summary.outputs} outputs and ${externals} lockfile packages:`,
  "",
  "| Command | Mean | ± | Min | Max |",
  "| --- | --: | --: | --: | --: |",
  ...measured.map(
    (result) =>
      `| ${result.command} | ${milliseconds(result.mean)} | ${milliseconds(result.stddev ?? 0)} | ${milliseconds(result.min)} | ${milliseconds(result.max)} |`,
  ),
  "",
].join("\n");
if (option("--json")) writeFileSync(option("--json"), `${JSON.stringify(summary, null, 2)}\n`);
if (option("--markdown")) writeFileSync(option("--markdown"), markdown);
console.log(`\n${markdown}`);
