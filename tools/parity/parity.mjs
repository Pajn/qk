#!/usr/bin/env node
// Captures Nx's view of a workspace as golden files, and compares qk against
// them. The goldens are the parity oracle: capture them once per Nx upgrade or
// workspace change, then compare on every qk change.
//
//   node tools/parity/parity.mjs compare [--qk <binary>]
//   node tools/parity/parity.mjs capture [--check]
//
// By default both work on the fixture in tools/parity/fixture, with the cases
// in tools/parity/cases.json and the goldens in tools/parity/goldens. Each run
// builds a throwaway git repository from the fixture: the fixture is the base
// commit, and each case with a `change` is a branch that writes the files in
// tools/parity/changes/<case> and deletes the paths it lists. Capture needs the
// Nx release in tools/parity/nx installed (`pnpm install` there); compare needs
// only the goldens. `capture --check` captures into a scratch directory and
// fails when the committed goldens differ, which is how an Nx upgrade or a
// fixture edit without recaptured goldens shows.
//
//   node tools/parity/parity.mjs compare --workspace <dir> --goldens <dir>
//   node tools/parity/parity.mjs capture --workspace <dir> --goldens <dir>
//
// point both at a real workspace with its own Nx instead, reading the cases
// from <goldens>/parity.json and revisions from the workspace's history.
//
// Cases:
//
//   "taskGraphs": { "<name>": [<run-many arguments>] }
//   "affected":   { "<name>": { "change": {"delete": []} } | { "base": "<rev>", "head": "<rev>" } }
//   "runs":       { "<name>": { "change": {}, "args": [<run-many arguments>] } }
//
// Task graphs compare `--graph` output: tasks, their targets, roots, outputs,
// cache, continuous and parallelism flags, and dependencies.
// Affected cases compare `show projects --affected` sets. Runs execute the
// tasks without cache and compare whether the run failed and which tasks ran,
// as recorded by the fixture's tasks. An affected or run case whose results
// differ fails unless it carries the difference it accepts and why:
//
//   "accepted": { "nxOnly": ["a"], "qkOnly": [], "reason": "a installs the same" }

import { spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// Known differences, applied to both sides before comparing:
// - `dynamic` edges: Nx derives them from `import()` in source files. qk builds
//   the graph from manifests only (see the design), so a dynamic edge is
//   ignored when a static edge covers the same pair, and reported otherwise.
// - Node `metadata` and target `metadata`: Nx plugin annotations.
// - Node `namedInputs`: qk reports the merged named inputs; Nx does not.
// - Node `includedScripts`: Nx keeps the package.json option; qk consumes it.
// - `nx-release-publish` targets: inferred by Nx release, which qk does not
//   implement.
// - Target keys Nx fills with defaults (`parallelism: true`, empty
//   `configurations` and `options`) and `//` comment keys, which qk drops.
// - Tokens in target options: Nx substitutes them while building the graph,
//   qk when running a task. qk's options are substituted as Nx does.
// - Object key order, which neither tool treats as meaningful.
// - The order of `show projects --affected`: Nx prints its traversal order, qk
//   the graph order of `show projects`, so affected projects compare as sets.
// - Requested configurations in task graphs: Nx's `--graph` ignores `-c`, even
//   for an explicit `project:target:configuration`, and plans as if none was
//   requested; default configurations still apply. qk plans the same arguments
//   without `-c` to match.
const RELEASE_TARGETS = new Set(["nx-release-publish"]);

const here = dirname(fileURLToPath(import.meta.url));
const [command, ...rest] = process.argv.slice(2);
const option = (name) => {
  const index = rest.indexOf(name);
  return index >= 0 ? rest[index + 1] : undefined;
};
if (!["capture", "compare"].includes(command)) {
  console.error("usage: parity.mjs capture [--check] | compare [--qk <binary>] [--workspace <dir> --goldens <dir>]");
  process.exit(2);
}
const external = option("--workspace");
const qk = option("--qk") ? resolve(option("--qk")) : "qk";
const checkOnly = rest.includes("--check");
let goldens = resolve(external ? option("--goldens") : join(here, "goldens"));
const config = JSON.parse(readFileSync(external ? join(goldens, "parity.json") : join(here, "cases.json"), "utf8"));
const taskGraphs = config.taskGraphs ?? {};
const affectedCases = config.affected ?? {};
const runCases = config.runs ?? {};
const inspections = config.inspections ?? {};
const scratchRoot = mkdtempSync(join(tmpdir(), "qk-parity-"));
process.on("exit", () => rmSync(scratchRoot, { recursive: true, force: true }));

// The workspace, and the revisions of each case.
const workspace = external ? resolve(external) : fixture();
const nx = external ? join(workspace, "node_modules/.bin/nx") : join(here, "nx/node_modules/.bin/nx");

function git(...args) {
  const result = spawnSync("git", args, {
    cwd: workspace,
    encoding: "utf8",
    env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null", GIT_CONFIG_NOSYSTEM: "1" },
  });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

function fixture() {
  const root = join(scratchRoot, "workspace");
  cpSync(join(here, "fixture"), root, { recursive: true });
  const modules = join(here, "nx/node_modules");
  if (existsSync(modules)) symlinkSync(modules, join(root, "node_modules"));
  return root;
}

const commit = (message) =>
  git("-c", "user.name=parity", "-c", "user.email=parity@example.invalid", "commit", "--quiet", "--allow-empty", "-m", message);

if (!external) {
  git("init", "--quiet", "--initial-branch=main");
  git("add", "--all");
  commit("fixture");
  for (const [name, spec] of [...Object.entries(affectedCases), ...Object.entries(runCases)]) {
    if (!spec.change) continue;
    git("checkout", "--quiet", "-b", `case/${name}`, "main");
    const overlay = join(here, "changes", name);
    if (existsSync(overlay)) cpSync(overlay, workspace, { recursive: true });
    for (const path of spec.change.delete ?? []) rmSync(join(workspace, path), { recursive: true, force: true });
    git("add", "--all");
    commit(name);
    git("checkout", "--quiet", "main");
  }
}

/// Checks out a case's head for the duration of `body`, as CI would.
function onCase(name, spec, body) {
  if (!spec.change) return body({ base: spec.base, head: spec.head });
  git("checkout", "--quiet", `case/${name}`);
  try {
    return body({ base: "main", head: `case/${name}` });
  } finally {
    git("checkout", "--quiet", "main");
  }
}

// Runs a command three times and reports the median wall time, so a cold
// file system cache on the first run does not skew the timings.
function run(binary, args, { times = 3, allowFailure = false } = {}) {
  const durations = [];
  let result;
  for (let attempt = 0; attempt < times; attempt++) {
    const started = performance.now();
    result = spawnSync(binary, args, {
      cwd: workspace,
      encoding: "utf8",
      maxBuffer: 1 << 30,
      env: { ...process.env, NX_DAEMON: "false", NX_NO_CLOUD: "true", NX_TUI: "false", FIXTURE_ROOT: workspace },
    });
    durations.push(performance.now() - started);
    if (result.status !== 0 && !allowFailure) {
      throw new Error(`${binary} ${args.join(" ")} exited ${result.status}\n${result.stdout}\n${result.stderr}`);
    }
  }
  durations.sort((a, b) => a - b);
  return { ...result, milliseconds: Math.round(durations[Math.floor(durations.length / 2)]) };
}

function readJson(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

const affectedArgs = ({ base, head }) => ["show", "projects", "--affected", "--base", base, "--head", head, "--json"];
const lastLine = (text) => text.trim().split("\n").pop();

/// Runs a case's tasks without cache and reports the outcome and the tasks
/// that ran, from the markers the fixture's tasks leave.
function execute(binary, args, skipCache) {
  rmSync(join(workspace, ".ran"), { recursive: true, force: true });
  const result = run(binary, ["run-many", ...args, skipCache], { times: 1, allowFailure: true });
  const markers = join(workspace, ".ran");
  const ran = existsSync(markers) ? readdirSync(markers).map((file) => readFileSync(join(markers, file), "utf8")).sort() : [];
  rmSync(markers, { recursive: true, force: true });
  return { outcome: result.status === 0 ? "success" : "failure", ran };
}

/// Execute the isolated cache/stdin/watch oracle against the selected tool.
function behavioral(binary, golden) {
  const result = spawnSync(process.execPath, [join(here, "behavior.mjs"), command, binary, golden], {
    encoding: "utf8", timeout: 180_000,
  });
  if (result.status !== 0) throw new Error(`behavioral parity failed: ${result.error ?? ""}\n${result.stdout}\n${result.stderr}`);
  process.stdout.write(result.stdout);
}

if (command === "capture") {
  if (!existsSync(nx)) throw new Error(`no Nx installation at ${nx}; run pnpm install in ${dirname(dirname(dirname(nx)))}`);
  const committed = goldens;
  if (checkOnly) goldens = join(scratchRoot, "goldens");
  mkdirSync(goldens, { recursive: true });
  const timings = {};
  const projects = run(nx, ["show", "projects", "--json"]);
  writeFileSync(join(goldens, "projects.json"), projects.stdout);
  timings["show projects"] = projects.milliseconds;
  const graphFile = join(goldens, "graph.json");
  timings.graph = run(nx, ["graph", `--file=${graphFile}`]).milliseconds;
  for (const [name, args] of Object.entries(taskGraphs)) {
    const file = join(goldens, `tasks-${name}.json`);
    timings[`tasks ${name}`] = run(nx, ["run-many", ...args, `--graph=${file}`]).milliseconds;
    // Keep only what compare reads, so the goldens stay small and stable.
    const { tasks } = readJson(file);
    const kept = keptTasks(tasks.tasks);
    writeFileSync(
      file,
      `${JSON.stringify({ tasks: { tasks: kept, dependencies: tasks.dependencies, continuousDependencies: tasks.continuousDependencies } }, null, 2)}\n`,
    );
  }
  for (const [name, spec] of Object.entries(inspections)) {
    const result = run(nx, spec.args);
    writeFileSync(join(goldens, `inspect-${name}.json`), `${JSON.stringify(inspectionFields(JSON.parse(result.stdout), spec.fields), null, 2)}\n`);
  }
  for (const [name, spec] of Object.entries(affectedCases)) {
    const affected = onCase(name, spec, (revisions) => run(nx, affectedArgs(revisions)));
    writeFileSync(join(goldens, `affected-${name}.json`), `${JSON.stringify(JSON.parse(lastLine(affected.stdout)).sort())}\n`);
    timings[`affected ${name}`] = affected.milliseconds;
  }
  for (const [name, spec] of Object.entries(runCases)) {
    const result = onCase(name, spec, () => execute(nx, spec.args, "--skip-nx-cache"));
    writeFileSync(join(goldens, `runs-${name}.json`), `${JSON.stringify(result, null, 2)}\n`);
  }
  if (!external) behavioral(nx, join(goldens, "behavior.json"));
  const { version } = readJson(join(dirname(dirname(nx)), "nx/package.json"));
  // An external workspace records timings as its Nx baseline; the fixture's
  // goldens are committed, so they hold nothing that varies between captures.
  const summary = external ? { version, timings } : { version };
  writeFileSync(join(goldens, "nx.json"), `${JSON.stringify(summary, null, 2)}\n`);
  if (checkOnly) {
    const stale = readdirSync(goldens).filter(
      (file) => !existsSync(join(committed, file)) || readFileSync(join(committed, file), "utf8") !== readFileSync(join(goldens, file), "utf8"),
    );
    const extra = readdirSync(committed).filter((file) => !existsSync(join(goldens, file)));
    if (stale.length + extra.length > 0) {
      console.error(`goldens are stale: ${[...stale, ...extra].join(", ")}; run capture and commit the result`);
      process.exit(1);
    }
    console.log(`goldens match Nx ${version}`);
  } else {
    console.log(`captured Nx ${version} goldens in ${goldens}`);
  }
  process.exit(0);
}

const failures = [];
const timings = {};
if (!external) behavioral(qk, join(goldens, "behavior.json"));
const nxTimings = readJson(join(goldens, "nx.json")).timings ?? {};

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

/// Sets that must match, or differ exactly as the case accepts.
function checkSets(label, expected, actual, accepted, extra = []) {
  const nxOnly = [...expected].filter((item) => !actual.has(item)).sort();
  const qkOnly = [...actual].filter((item) => !expected.has(item)).sort();
  const same = (one, other) => JSON.stringify(one) === JSON.stringify([...(other ?? [])].sort());
  if (accepted?.reason && extra.length === 0 && nxOnly.length + qkOnly.length > 0 && same(nxOnly, accepted.nxOnly) && same(qkOnly, accepted.qkOnly)) {
    console.log(`ok   ${label} differs as accepted: ${accepted.reason}`);
    return;
  }
  check(label, [...extra, ...nxOnly.map((item) => `only in Nx: ${item}`), ...qkOnly.map((item) => `only in qk: ${item}`)]);
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
const graphFile = join(scratchRoot, "graph.json");
timings.graph = run(qk, ["graph", "--file", graphFile]).milliseconds;
check("graph --file is normalised-equal", compareGraphs(readJson(join(goldens, "graph.json")).graph, readJson(graphFile).graph));

// Task graphs: the same tasks, dependencies, and cache and continuous flags.
for (const [name, args] of Object.entries(taskGraphs)) {
  const plan = run(qk, ["run-many", ...withoutConfiguration(args), "--graph=stdout"]);
  timings[`tasks ${name}`] = plan.milliseconds;
  check(`task graph ${name} matches`, compareTaskGraphs(readJson(join(goldens, `tasks-${name}.json`)).tasks, JSON.parse(plan.stdout).tasks));
}

// Inspect the fields shared by both standalone configuration models.
for (const [name, spec] of Object.entries(inspections)) {
  const result = run(qk, spec.args);
  check(`inspection ${name} matches`, diffValues(readJson(join(goldens, `inspect-${name}.json`)), inspectionFields(JSON.parse(result.stdout), spec.fields)));
}

// Affected projects: the same set, or exactly the accepted difference.
for (const [name, spec] of Object.entries(affectedCases)) {
  const affected = onCase(name, spec, (revisions) => run(qk, affectedArgs(revisions)));
  timings[`affected ${name}`] = affected.milliseconds;
  checkSets(`affected ${name}`, new Set(readJson(join(goldens, `affected-${name}.json`))), new Set(JSON.parse(affected.stdout)), spec.accepted);
}

// Runs: the same outcome and the same tasks run.
for (const [name, spec] of Object.entries(runCases)) {
  const expected = readJson(join(goldens, `runs-${name}.json`));
  const actual = onCase(name, spec, () => execute(qk, spec.args, "--skip-cache"));
  const outcome = expected.outcome === actual.outcome ? [] : [`run ${actual.outcome === "success" ? "succeeded" : "failed"} in qk only`];
  checkSets(`run ${name}`, new Set(expected.ran), new Set(actual.ran), spec.accepted, outcome);
}

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

/// run-many arguments without a requested configuration.
function withoutConfiguration(args) {
  const kept = [];
  for (let index = 0; index < args.length; index++) {
    if (["-c", "--configuration"].includes(args[index])) index++;
    else if (!args[index].startsWith("--configuration=")) kept.push(args[index]);
  }
  return kept;
}

// What task graphs compare: each task's target, root, outputs and flags.
function keptTasks(tasks) {
  return Object.fromEntries(
    Object.entries(tasks).map(([id, task]) => [
      id,
      {
        target: task.target,
        projectRoot: task.projectRoot,
        outputs: task.outputs,
        cache: Boolean(task.cache),
        continuous: Boolean(task.continuous),
        parallelism: task.parallelism !== false,
      },
    ]),
  );
}

// Both sides are `--graph` task graphs: the same tasks, with the same kept
// fields, dependencies and continuous dependencies.
function compareTaskGraphs(nx, qk) {
  const differences = [];
  diffKeys("task", Object.keys(nx.tasks), Object.keys(qk.tasks), differences);
  const ours = keptTasks(qk.tasks);
  for (const [id, task] of Object.entries(nx.tasks)) {
    if (!ours[id]) continue;
    for (const [field, value] of Object.entries(task)) {
      if (JSON.stringify(canonical(value)) !== JSON.stringify(canonical(ours[id][field]))) {
        differences.push(`${id}: ${field} ${JSON.stringify(value)} in Nx, ${JSON.stringify(ours[id][field])} in qk`);
      }
    }
    for (const kind of ["dependencies", "continuousDependencies"]) {
      const expected = [...(nx[kind]?.[id] ?? [])].sort();
      const actual = [...(qk[kind]?.[id] ?? [])].sort();
      if (JSON.stringify(expected) !== JSON.stringify(actual)) {
        differences.push(`${id}: ${kind} ${JSON.stringify(expected)} in Nx, ${JSON.stringify(actual)} in qk`);
      }
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

// Nx Cloud injects this input even with NX_NO_CLOUD; distributed caching is
// outside qk's scope. Keep declared categories and omit empty arrays.
function inspectionFields(value, fields) {
  return Object.fromEntries(fields.flatMap((key) => {
    const item = key === "environment" && Array.isArray(value[key])
      ? value[key].filter((name) => name !== "NX_CLOUD_ENCRYPTION_KEY")
      : value[key];
    return item === undefined || (Array.isArray(item) && item.length === 0) ? [] : [[key, item]];
  }));
}
