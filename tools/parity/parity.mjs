#!/usr/bin/env node
// Captures Nx's view of a workspace as golden files, and compares qk against
// them. The goldens are the parity oracle of the design's phase 0: capture them
// once per Nx upgrade or workspace change, then compare on every qk change.
//
//   node tools/parity/parity.mjs capture <workspace> <goldens>
//   node tools/parity/parity.mjs compare <workspace> <goldens> [--qk <binary>]
//
// Capture runs the workspace's own Nx (node_modules/.bin/nx) with the daemon
// off. `<goldens>/parity.json` names the task graphs to capture, each as the
// run-many arguments both tools accept:
//
//   { "taskGraphs": { "check-all": ["--exclude=js", "-t", "tsc", "test"] } }
//
// and the revision pairs to capture `show projects --affected` for:
//
//   { "affected": { "lockfile-bump": { "base": "<sha>", "head": "<sha>" } } }
//
// An affected case whose sets differ fails unless it carries the difference
// it accepts and why, which is how precision improvements are recorded:
//
//   "accepted": { "nxOnly": ["a"], "qkOnly": [], "reason": "a installs the same" }
//
// Compare exits nonzero on any divergence not listed under "Known
// differences" below, or accepted by an affected case.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Known differences, applied to both sides before comparing:
// - `dynamic` edges: Nx derives them from `import()` in source files. qk builds
//   the graph from manifests only (see the design), so a dynamic edge is
//   ignored when a static edge covers the same pair, and reported otherwise.
// - Node `metadata` and target `metadata`: Nx plugin annotations.
// - Node `namedInputs`: qk reports the merged named inputs; Nx does not.
// - Node `includedScripts`: Nx keeps the package.json option; qk consumes it.
// - `nx-release-publish` targets: inferred by Nx release, which stays on Nx
//   until phase 6.
// - Target keys Nx fills with defaults (`parallelism: true`, empty
//   `configurations` and `options`) and `//` comment keys, which qk drops.
// - Tokens in target options: Nx substitutes them while building the graph,
//   qk when running a task. qk's options are substituted as Nx does.
// - Object key order, which neither tool treats as meaningful.
// - The order of `show projects --affected`: Nx prints its traversal order, qk
//   the graph order of `show projects`, so affected projects compare as sets.
// - Configurations in task graphs: Nx's `--graph` leaves them out of task ids
//   even for an explicit `project:target:configuration`, so qk's are dropped
//   before comparing.
const RELEASE_TARGETS = new Set(["nx-release-publish"]);

const [command, workspaceArg, goldensArg, ...rest] = process.argv.slice(2);
if (!["capture", "compare"].includes(command) || !workspaceArg || !goldensArg) {
  console.error("usage: parity.mjs capture|compare <workspace> <goldens> [--qk <binary>]");
  process.exit(2);
}
const workspace = resolve(workspaceArg);
const goldens = resolve(goldensArg);
const qkIndex = rest.indexOf("--qk");
const qk = qkIndex >= 0 ? resolve(rest[qkIndex + 1]) : "qk";
const config = JSON.parse(readFileSync(join(goldens, "parity.json"), "utf8"));
const taskGraphs = config.taskGraphs ?? {};
const affectedCases = config.affected ?? {};
const affectedArgs = ({ base, head }) => ["show", "projects", "--affected", "--base", base, "--head", head, "--json"];

// Runs a command three times and reports the median wall time, so a cold
// file system cache on the first run does not skew the timings.
function run(binary, args) {
  const times = [];
  let result;
  for (let attempt = 0; attempt < 3; attempt++) {
    const started = performance.now();
    result = spawnSync(binary, args, {
      cwd: workspace,
      encoding: "utf8",
      maxBuffer: 1 << 30,
      env: { ...process.env, NX_DAEMON: "false" },
    });
    times.push(performance.now() - started);
    if (result.status !== 0) {
      throw new Error(`${binary} ${args.join(" ")} exited ${result.status}\n${result.stderr}`);
    }
  }
  return { stdout: result.stdout, milliseconds: Math.round(times.sort((a, b) => a - b)[1]) };
}

function scratch(name) {
  const directory = join(tmpdir(), `qk-parity-${process.pid}`);
  mkdirSync(directory, { recursive: true });
  return join(directory, name);
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

if (command === "capture") {
  const nx = join(workspace, "node_modules/.bin/nx");
  if (!existsSync(nx)) throw new Error(`no Nx installation at ${nx}`);
  const timings = {};
  const projects = run(nx, ["show", "projects", "--json"]);
  writeFileSync(join(goldens, "projects.json"), projects.stdout);
  timings["show projects"] = projects.milliseconds;
  const graphFile = join(goldens, "graph.json");
  timings.graph = run(nx, ["graph", `--file=${graphFile}`]).milliseconds;
  for (const [name, args] of Object.entries(taskGraphs)) {
    const file = join(goldens, `tasks-${name}.json`);
    timings[`tasks ${name}`] = run(nx, ["run-many", ...args, `--graph=${file}`]).milliseconds;
  }
  for (const [name, revisions] of Object.entries(affectedCases)) {
    const affected = run(nx, affectedArgs(revisions));
    const projects = JSON.parse(affected.stdout.trim().split("\n").pop()).sort();
    writeFileSync(join(goldens, `affected-${name}.json`), `${JSON.stringify(projects)}\n`);
    timings[`affected ${name}`] = affected.milliseconds;
  }
  const { version } = readJson(join(workspace, "node_modules/nx/package.json"));
  writeFileSync(join(goldens, "nx.json"), `${JSON.stringify({ version, timings }, null, 2)}\n`);
  console.log(`captured Nx ${version} goldens in ${goldens}`);
  process.exit(0);
}

const failures = [];
const timings = {};
const nxTimings = readJson(join(goldens, "nx.json")).timings;

function check(label, differences) {
  if (differences.length === 0) {
    console.log(`ok   ${label}`);
    return;
  }
  console.log(`FAIL ${label}: ${differences.length} differences`);
  for (const difference of differences.slice(0, 20)) console.log(`       ${difference}`);
  if (differences.length > 20) console.log(`       ... ${differences.length - 20} more`);
  failures.push(label);
}

// show projects: byte-identical.
const projects = run(qk, ["show", "projects", "--json"]);
timings["show projects"] = projects.milliseconds;
check(
  "show projects --json is byte-identical",
  projects.stdout === readFileSync(join(goldens, "projects.json"), "utf8")
    ? []
    : ["output differs; diff projects.json against `qk show projects --json`"],
);

// graph: normalised-equal.
const graphFile = scratch("graph.json");
timings.graph = run(qk, ["graph", "--file", graphFile]).milliseconds;
check("graph --file is normalised-equal", compareGraphs(readJson(join(goldens, "graph.json")).graph, readJson(graphFile).graph));

// Task graphs: the same tasks, dependencies, and cache and continuous flags.
for (const [name, args] of Object.entries(taskGraphs)) {
  const plan = run(qk, ["run-many", ...args, "--dry-run"]);
  timings[`tasks ${name}`] = plan.milliseconds;
  check(`task graph ${name} matches`, compareTaskGraphs(readJson(join(goldens, `tasks-${name}.json`)).tasks, JSON.parse(plan.stdout)));
}

// Affected projects: the same set, or exactly the accepted difference.
for (const [name, revisions] of Object.entries(affectedCases)) {
  const affected = run(qk, affectedArgs(revisions));
  timings[`affected ${name}`] = affected.milliseconds;
  const expected = new Set(readJson(join(goldens, `affected-${name}.json`)));
  const actual = new Set(JSON.parse(affected.stdout));
  const nxOnly = [...expected].filter((project) => !actual.has(project)).sort();
  const qkOnly = [...actual].filter((project) => !expected.has(project)).sort();
  const accepted = revisions.accepted;
  const same = (one, other) => JSON.stringify(one) === JSON.stringify([...(other ?? [])].sort());
  if (accepted?.reason && same(nxOnly, accepted.nxOnly) && same(qkOnly, accepted.qkOnly) && nxOnly.length + qkOnly.length > 0) {
    console.log(`ok   affected ${name} differs as accepted: ${accepted.reason}`);
    continue;
  }
  check(`affected ${name} matches`, [
    ...nxOnly.map((project) => `only in Nx: ${project}`),
    ...qkOnly.map((project) => `only in qk: ${project}`),
  ]);
}

rmSync(join(tmpdir(), `qk-parity-${process.pid}`), { recursive: true, force: true });
console.log(`\n${"milliseconds".padEnd(32)}     nx      qk`);
for (const [label, milliseconds] of Object.entries(timings)) {
  console.log(`${label.padEnd(32)} ${String(nxTimings[label] ?? "-").padStart(6)}  ${String(milliseconds).padStart(6)}`);
}
process.exit(failures.length === 0 ? 0 : 1);

function compareGraphs(nx, qk) {
  const differences = [];
  diffKeys("node", Object.keys(nx.nodes), Object.keys(qk.nodes), differences);
  for (const name of Object.keys(nx.nodes)) {
    if (!qk.nodes[name]) continue;
    const expected = normaliseNode(nx.nodes[name], false);
    const actual = normaliseNode(qk.nodes[name], true);
    for (const difference of diffValues(expected, actual)) differences.push(`${name}${difference}`);
  }
  const edges = (graph) =>
    new Set(Object.values(graph.dependencies).flatMap((list) => list.map((edge) => `${edge.source} -> ${edge.target} (${edge.type})`)));
  const nxEdges = edges(nx);
  const qkEdges = edges(qk);
  for (const edge of nxEdges) {
    if (qkEdges.has(edge)) continue;
    if (edge.endsWith("(dynamic)") && nxEdges.has(edge.replace("(dynamic)", "(static)"))) continue;
    differences.push(`edge only in Nx: ${edge}`);
  }
  for (const edge of qkEdges) {
    if (!nxEdges.has(edge)) differences.push(`edge only in qk: ${edge}`);
  }
  return differences;
}

function normaliseNode(node, isQk) {
  const { metadata, namedInputs, includedScripts, targets = {}, ...data } = node.data;
  const normalisedTargets = {};
  for (const [name, target] of Object.entries(targets)) {
    if (RELEASE_TARGETS.has(name)) continue;
    normalisedTargets[name] = normaliseTarget(target, node.data, isQk);
  }
  return { name: node.name, type: node.type, data: { ...data, targets: normalisedTargets } };
}

function normaliseTarget(target, project, isQk) {
  const result = {};
  for (const [key, value] of Object.entries(target)) {
    if (key === "metadata" || key.startsWith("//")) continue;
    if (key === "parallelism" && value === true) continue;
    if (["options", "configurations"].includes(key) && Object.keys(value).length === 0) continue;
    result[key] = isQk && ["options", "configurations"].includes(key) ? resolveTokens(value, project) : value;
  }
  return result;
}

// Nx's resolveNxTokensInOptions.
function resolveTokens(value, project) {
  if (typeof value === "string") {
    return value
      .replace(/^\{workspaceRoot\}\/?/, "")
      .replaceAll("{projectRoot}", project.root)
      .replaceAll("{projectName}", project.name);
  }
  if (Array.isArray(value)) return value.map((item) => resolveTokens(item, project));
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolveTokens(item, project)]));
  }
  return value;
}

function compareTaskGraphs(nx, planned) {
  const withoutConfiguration = (id) => {
    const task = planned.tasks[id];
    return task.configuration ? id.slice(0, -(task.configuration.length + 1)) : id;
  };
  const qk = { tasks: {} };
  for (const [id, task] of Object.entries(planned.tasks)) {
    qk.tasks[withoutConfiguration(id)] = { ...task, dependencies: task.dependencies.map(withoutConfiguration) };
  }
  const differences = [];
  diffKeys("task", Object.keys(nx.tasks), Object.keys(qk.tasks), differences);
  for (const [id, task] of Object.entries(nx.tasks)) {
    const ours = qk.tasks[id];
    if (!ours) continue;
    const expected = [...(nx.dependencies[id] ?? []), ...(nx.continuousDependencies?.[id] ?? [])].sort();
    const actual = [...ours.dependencies].sort();
    for (const dependency of expected) {
      if (!actual.includes(dependency)) differences.push(`${id}: dependency only in Nx: ${dependency}`);
    }
    for (const dependency of actual) {
      if (!expected.includes(dependency)) differences.push(`${id}: dependency only in qk: ${dependency}`);
    }
    if (task.cache !== Boolean(ours.definition.cache)) differences.push(`${id}: cache ${task.cache} in Nx`);
    if (Boolean(task.continuous) !== Boolean(ours.definition.continuous)) {
      differences.push(`${id}: continuous ${Boolean(task.continuous)} in Nx`);
    }
  }
  return differences;
}

function diffKeys(kind, expected, actual, differences) {
  const ours = new Set(actual);
  const theirs = new Set(expected);
  for (const key of expected) if (!ours.has(key)) differences.push(`${kind} only in Nx: ${key}`);
  for (const key of actual) if (!theirs.has(key)) differences.push(`${kind} only in qk: ${key}`);
}

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, canonical(value[key])]),
    );
  }
  return value;
}

function diffValues(expected, actual, path = "") {
  if (JSON.stringify(canonical(expected)) === JSON.stringify(canonical(actual))) return [];
  const bothObjects = [expected, actual].every((value) => value && typeof value === "object" && !Array.isArray(value));
  if (!bothObjects) {
    return [`${path}: Nx ${JSON.stringify(expected)}, qk ${JSON.stringify(actual)}`];
  }
  return [...new Set([...Object.keys(expected), ...Object.keys(actual)])].flatMap((key) =>
    diffValues(expected[key], actual[key], `${path}.${key}`),
  );
}
