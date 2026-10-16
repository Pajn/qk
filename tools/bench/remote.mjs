#!/usr/bin/env node
// Benchmarks restoring a cached chain of tasks from a remote store.
//
// A local stand-in for S3 answers each request after a fixed latency and
// sends bodies at a fixed bandwidth, as a store across a network would. One
// machine builds a chain of projects, each output holding small text files and
// one large file that does not compress, and uploads it. Another restores it:
//
// - cold: nothing local, as on a fresh CI runner;
// - held: the local cache holds every file but no entry, as on a developer's
//   machine after pulling a commit another machine built.
//
// Each cell is the median of several runs, with the outputs checked after
// each, outside the timing. With --gate, it fails when restoring cold with the
// default large-blob threshold takes more than 5% longer than with every blob
// inline, which costs what a qk/v3 pack did, at 230 ms and 200 Mbps.
//
// node tools/bench/remote.mjs --qk target/release/qk [--full] [--gate]
//   [--runs 3] [--depth 8] [--large-kb 4096] [--json out.json] [--markdown out.md]

import { spawn, spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseArgs } from "node:util";

const { values: options } = parseArgs({
  options: {
    qk: { type: "string" },
    full: { type: "boolean", default: false },
    gate: { type: "boolean", default: false },
    runs: { type: "string", default: "3" },
    depth: { type: "string", default: "8" },
    "large-kb": { type: "string", default: "4096" },
    json: { type: "string" },
    markdown: { type: "string" },
  },
});
if (!options.qk) {
  console.error("usage: remote.mjs --qk <path to qk> [--full] [--gate]");
  process.exit(2);
}
const qk = resolve(options.qk);
const runs = Number(options.runs);
const depth = Number(options.depth);
const largeBytes = Number(options["large-kb"]) * 1024;

// The stand-in store: path-style objects in memory, signatures ignored.
function store() {
  const objects = new Map();
  const network = { latency: 0, bytesPerSecond: Infinity };
  const send = (response, status, body = Buffer.alloc(0), head = false) => {
    setTimeout(() => {
      response.writeHead(status, { "Content-Length": body.length });
      if (head || body.length === 0) return response.end();
      const chunk = 64 * 1024;
      let at = 0;
      const next = () => {
        const part = body.subarray(at, at + chunk);
        at += part.length;
        const done = at >= body.length;
        response.write(part);
        if (done) return response.end();
        const wait = network.bytesPerSecond === Infinity ? 0 : (part.length / network.bytesPerSecond) * 1000;
        setTimeout(next, wait);
      };
      next();
    }, network.latency);
  };
  const server = createServer((request, response) => {
    const chunks = [];
    request.on("data", (chunk) => chunks.push(chunk));
    request.on("end", () => {
      const url = new URL(request.url, "http://store");
      const path = decodeURIComponent(url.pathname);
      const body = Buffer.concat(chunks);
      if (request.method === "GET" && url.searchParams.has("list-type")) {
        const prefix = `${path.replace(/\/$/, "")}/${url.searchParams.get("prefix") ?? ""}`;
        const bucket = `${path.replace(/\/$/, "")}/`;
        const keys = [...objects.keys()].filter((key) => key.startsWith(prefix)).sort();
        const xml =
          '<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">' +
          keys
            .map((key) => `<Contents><Key>${key.slice(bucket.length)}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified><ETag>"x"</ETag><Size>0</Size></Contents>`)
            .join("") +
          "</ListBucketResult>";
        return send(response, 200, Buffer.from(xml));
      }
      if (request.method === "POST" && url.searchParams.has("delete")) {
        const bucket = path.replace(/\/$/, "");
        for (const match of body.toString().matchAll(/<Key>(.*?)<\/Key>/g)) objects.delete(`${bucket}/${match[1]}`);
        return send(response, 200, Buffer.from('<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"></DeleteResult>'));
      }
      if (request.method === "PUT") {
        objects.set(path, body);
        return send(response, 200);
      }
      if (request.method === "GET" || request.method === "HEAD") {
        const object = objects.get(path);
        return object ? send(response, 200, object, request.method === "HEAD") : send(response, 404);
      }
      send(response, 405);
    });
  });
  return { server, objects, network };
}

// Deterministic bytes that do not compress.
function noise(length, seed) {
  const bytes = Buffer.alloc(length);
  let state = BigInt(seed) * 6364136223846793005n + 1442695040888963407n;
  for (let index = 0; index < length; index++) {
    state = (state * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
    bytes[index] = Number(state >> 56n);
  }
  return bytes;
}

function workspace(endpoint) {
  const root = mkdtempSync(join(tmpdir(), "qk-remote-bench-"));
  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "bench", private: true }));
  writeFileSync(join(root, ".gitignore"), "dist/\n.qk/\nlarge/\n");
  writeFileSync(
    join(root, "nx.json"),
    JSON.stringify({
      s3: { bucket: "cache", region: "us-east-1", endpoint, forcePathStyle: true },
      targetDefaults: { build: { cache: true, outputs: ["{projectRoot}/dist"], dependsOn: ["^build"] } },
    }),
  );
  mkdirSync(join(root, "large"));
  writeFileSync(
    join(root, "build.mjs"),
    `import { cpSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
// Commands run from the workspace root.
const index = process.argv[2];
const counter = ".qk-builds";
writeFileSync(counter, String((existsSync(counter) ? Number(readFileSync(counter, "utf8")) : 0) + 1));
const dist = join("packages", "p" + index, "dist");
mkdirSync(dist, { recursive: true });
for (let file = 0; file < 40; file++) writeFileSync(join(dist, "file" + file + ".js"), ("export const v" + file + " = " + index + ";\\n").repeat(200));
cpSync(join("large", index + ".bin"), join(dist, "large.bin"));
`,
  );
  for (let index = 0; index < depth; index++) {
    const project = join(root, "packages", `p${index}`);
    mkdirSync(join(project, "src"), { recursive: true });
    writeFileSync(join(project, "src/index.js"), `export const index = ${index};\n`);
    writeFileSync(
      join(project, "project.json"),
      JSON.stringify({
        name: `p${index}`,
        implicitDependencies: index ? [`p${index - 1}`] : [],
        targets: { build: { command: `node build.mjs ${index}` } },
      }),
    );
    writeFileSync(join(root, "large", `${index}.bin`), noise(largeBytes, index));
  }
  const git = (...args) => spawnSync("git", args, { cwd: root, stdio: "ignore" });
  git("init", "-q");
  git("add", "-A");
  git("-c", "user.name=bench", "-c", "user.email=bench@example.invalid", "commit", "-qm", "bench");
  return root;
}

// The store runs in this process, so qk must run without blocking it.
function qkRun(root, env) {
  const quiet = { ...process.env };
  delete quiet.CI;
  delete quiet.GITHUB_ACTIONS;
  return new Promise((done, failed) => {
    const started = process.hrtime.bigint();
    const child = spawn(qk, ["run", `p${depth - 1}:build`], {
      cwd: root,
      env: { ...quiet, AWS_ACCESS_KEY_ID: "bench", AWS_SECRET_ACCESS_KEY: "bench", ...env },
    });
    let output = "";
    child.stdout.on("data", (chunk) => (output += chunk));
    child.stderr.on("data", (chunk) => (output += chunk));
    child.on("error", failed);
    child.on("close", (status) => {
      const seconds = Number(process.hrtime.bigint() - started) / 1e9;
      if (status !== 0) failed(new Error(`qk failed:\n${output}`));
      else done(seconds);
    });
  });
}

function cachePath(root) {
  return spawnSync(qk, ["cache", "path"], { cwd: root, encoding: "utf8" }).stdout.trim();
}

const builds = (root) => Number(readFileSync(join(root, ".qk-builds"), "utf8"));

function clearOutputs(root) {
  for (let index = 0; index < depth; index++) rmSync(join(root, "packages", `p${index}`, "dist"), { recursive: true, force: true });
}

function verify(root, expectedBuilds) {
  if (builds(root) !== expectedBuilds) throw new Error("a restore rebuilt a project");
  for (let index = 0; index < depth; index++) {
    const size = statSync(join(root, "packages", `p${index}`, "dist", "large.bin")).size;
    if (size !== largeBytes) throw new Error(`p${index}'s large file was not restored`);
  }
}

const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
};

async function cell({ threshold, latency, mbps, prefetch, scenario }) {
  const { server, objects, network } = store();
  await new Promise((done) => server.listen(0, "127.0.0.1", done));
  const endpoint = `http://127.0.0.1:${server.address().port}`;
  const root = workspace(endpoint);
  const env = { QK_REMOTE_PREFETCH: prefetch ? "on" : "off" };
  if (threshold !== "default") env.QK_REMOTE_BLOB_THRESHOLD = threshold;
  try {
    await qkRun(root, env);
    const built = builds(root);
    const bytes = [...objects.values()].reduce((sum, object) => sum + object.length, 0);
    network.latency = latency;
    network.bytesPerSecond = (mbps * 1e6) / 8;
    const times = [];
    for (let run = 0; run < runs; run++) {
      const cache = cachePath(root);
      if (scenario === "cold") rmSync(cache, { recursive: true, force: true });
      else rmSync(join(cache, "entries"), { recursive: true, force: true });
      clearOutputs(root);
      const before = objects.size;
      times.push(await qkRun(root, env));
      verify(root, built);
      if (objects.size < before) throw new Error("a restore removed remote objects");
    }
    return { threshold, latency, mbps, prefetch, scenario, seconds: median(times), stored: bytes };
  } finally {
    server.close();
    rmSync(root, { recursive: true, force: true });
  }
}

const cells = [];
const add = (cell) => cells.push(cell);
if (options.full) {
  for (const threshold of ["off", "default", "16MB", "4MB", "1MB", "256KB"])
    for (const latency of [20, 230])
      for (const mbps of [200, 1000])
        for (const prefetch of [true, false])
          for (const scenario of ["cold", "held"]) add({ threshold, latency, mbps, prefetch, scenario });
} else {
  for (const threshold of ["off", "default", "256KB"])
    for (const scenario of ["cold", "held"]) add({ threshold, latency: 230, mbps: 200, prefetch: true, scenario });
  add({ threshold: "default", latency: 230, mbps: 200, prefetch: false, scenario: "cold" });
}

const results = [];
for (const spec of cells) {
  const result = await cell(spec);
  results.push(result);
  console.error(
    `${result.scenario} threshold=${result.threshold} ${result.latency}ms ${result.mbps}Mbps prefetch=${result.prefetch}: ${result.seconds.toFixed(2)}s`,
  );
}

const table = [
  "| Scenario | Threshold | Latency | Bandwidth | Prefetch | Median |",
  "| --- | --- | --- | --- | --- | --- |",
  ...results.map(
    (result) =>
      `| ${result.scenario} | ${result.threshold} | ${result.latency} ms | ${result.mbps} Mbps | ${result.prefetch ? "on" : "off"} | ${result.seconds.toFixed(2)} s |`,
  ),
].join("\n");
console.log(table);
if (options.json) writeFileSync(options.json, JSON.stringify({ depth, largeBytes, runs, results }, null, 2));
if (options.markdown)
  writeFileSync(
    options.markdown,
    `### Remote restoration\n\nA chain of ${depth} projects, each with a ${largeBytes >> 10} KiB file that does not compress.\n\n${table}\n`,
  );

if (options.gate) {
  const find = (threshold) =>
    results.find(
      (result) =>
        result.scenario === "cold" && result.threshold === threshold && result.latency === 230 && result.mbps === 200 && result.prefetch,
    );
  const inline = find("off");
  const chosen = find("default");
  if (chosen.seconds > inline.seconds * 1.05) {
    console.error(
      `gate: restoring cold with the default threshold took ${chosen.seconds.toFixed(2)}s, more than 5% over ${inline.seconds.toFixed(2)}s with every blob inline`,
    );
    process.exit(1);
  }
  console.error("gate: passed");
}
