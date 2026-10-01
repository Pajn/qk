// Measure broad output globs and verify their selected paths on every run.
// node tools/bench/output-globs.mjs --qk target/release/qk --baseline /path/to/previous/qk
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(name);
  return index < 0 ? fallback : args[index + 1];
};
const projects = Number(option("--projects", "100"));
const runs = Number(option("--runs", "9"));
if (!Number.isInteger(projects) || projects < 1 || !Number.isInteger(runs) || runs < 1) {
  throw new Error("--projects and --runs must be positive integers");
}
const binaries = [{ name: "qk", path: resolve(option("--qk", "target/release/qk")) }];
if (args.includes("--baseline")) {
  binaries.unshift({ name: "baseline", path: resolve(option("--baseline")) });
}
const root = mkdtempSync(join(tmpdir(), "qk-output-globs-"));
const write = (path, contents) => {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), contents);
};
try {
  write("nx.json", "{}");
  write(".gitignore", "node_modules/\n.qk/\n");
  write("package.json", JSON.stringify({ name: "js", nx: { targets: {
    "graphox-codegen": { command: "true", outputs: [
      "{workspaceRoot}/apps/*/graphql/manifest.json",
      "{workspaceRoot}/apps/*/*/graphql/manifest.json",
      "{workspaceRoot}/apps/*/*/*/graphql/manifest.json",
    ] },
  } } }));
  const expected = [];
  for (let project = 0; project < projects; project++) {
    for (const directory of ["graphql", "app/graphql", "cf-worker/src/graphql"]) {
      const path = `apps/p${project}/${directory}/manifest.json`;
      write(path, "{}");
      expected.push(path);
    }
    for (let dependency = 0; dependency < 30; dependency++) {
      for (let file = 0; file < 10; file++) {
        write(`apps/p${project}/node_modules/dep${dependency}/file${file}.js`, "export {};");
      }
    }
  }
  expected.sort();
  const measure = (binary) => {
    const started = performance.now();
    const result = spawnSync(binary.path, ["--workspace", root, "show", "target", "outputs", "js:graphox-codegen", "--json"], { encoding: "utf8" });
    const milliseconds = performance.now() - started;
    if (result.error || result.status !== 0) {
      throw new Error(`${binary.name}: ${result.error ?? result.stderr}`);
    }
    const actual = JSON.parse(result.stdout).expandedOutputs.sort();
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      throw new Error(`${binary.name}: output selection differs`);
    }
    return milliseconds;
  };
  const samples = new Map(binaries.map((binary) => [binary.name, []]));
  for (const binary of binaries) measure(binary);
  for (let run = 0; run < runs; run++) {
    const order = run % 2 === 0 ? binaries : [...binaries].reverse();
    for (const binary of order) samples.get(binary.name).push(measure(binary));
  }
  const results = Object.fromEntries([...samples].map(([name, times]) => {
    const sorted = [...times].sort((a, b) => a - b);
    const middle = Math.floor(sorted.length / 2);
    const median = sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
    return [name, { median_ms: median, samples_ms: times }];
  }));
  const report = { projects, outputs: expected.length, runs, results };
  console.log(JSON.stringify(report, null, 2));
  if (args.includes("--json")) writeFileSync(resolve(option("--json")), JSON.stringify(report, null, 2));
} finally {
  rmSync(root, { recursive: true, force: true });
}
