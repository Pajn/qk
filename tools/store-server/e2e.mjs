#!/usr/bin/env node
// Runs qk against this Worker under `wrangler dev`, so the client and the
// server are checked against each other rather than each against its own
// reading of PROTOCOL.md.
//
// node e2e.mjs --qk <path to qk>

import { spawn, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

const { values } = parseArgs({ options: { qk: { type: "string" }, port: { type: "string", default: "8799" } } });
if (!values.qk) {
  console.error("usage: e2e.mjs --qk <path to qk>");
  process.exit(2);
}
const qk = resolve(values.qk);
const here = dirname(fileURLToPath(import.meta.url));
const url = `http://127.0.0.1:${values.port}`;
const digest = (token) => createHash("sha256").update(token).digest("hex");
const write = "write-e2e";
const read = "read-e2e";

const wrangler = spawn(
  "pnpm",
  ["exec", "wrangler", "dev", "--local", "--port", values.port, "--var", `READ_TOKENS:${digest(read)}`, "--var", `WRITE_TOKENS:${digest(write)}`],
  { cwd: here, stdio: ["ignore", "pipe", "pipe"] },
);
let log = "";
wrangler.stdout.on("data", (chunk) => (log += chunk));
wrangler.stderr.on("data", (chunk) => (log += chunk));

const root = mkdtempSync(join(tmpdir(), "qk-store-e2e-"));
let failed = false;
const check = (condition, message) => {
  if (!condition) {
    failed = true;
    console.error(`FAIL: ${message}`);
  } else console.error(`ok: ${message}`);
};

try {
  for (let attempt = 0; ; attempt++) {
    const ready = await fetch(`${url}/v4/index`, { headers: { Authorization: `Bearer ${read}` } }).catch(() => null);
    if (ready?.status === 200) break;
    if (attempt > 120) throw new Error(`wrangler dev did not start:\n${log}`);
    await new Promise((done) => setTimeout(done, 500));
  }

  writeFileSync(join(root, "package.json"), JSON.stringify({ name: "e2e", private: true }));
  writeFileSync(join(root, ".gitignore"), "dist/\n");
  writeFileSync(
    join(root, "nx.json"),
    JSON.stringify({ storeServer: { url }, targetDefaults: { build: { cache: true, outputs: ["{projectRoot}/dist"] } } }),
  );
  mkdirSync(join(root, "app"));
  writeFileSync(join(root, "app/large.bin"), randomBytes(2_000_000));
  writeFileSync(
    join(root, "app/project.json"),
    JSON.stringify({
      name: "app",
      targets: { build: { command: "mkdir -p app/dist && cp app/large.bin app/dist/large.bin && echo small > app/dist/small.txt" } },
    }),
  );
  const git = (...args) => spawnSync("git", args, { cwd: root, stdio: "ignore" });
  git("init", "-q");
  git("add", "-A");
  git("-c", "user.name=e2e", "-c", "user.email=e2e@example.invalid", "commit", "-qm", "e2e");

  // Outside CI, qk reports each cache hit and miss, which the checks read.
  const quiet = { ...process.env };
  delete quiet.CI;
  delete quiet.GITHUB_ACTIONS;
  const run = (token, env = {}) =>
    spawnSync(qk, ["run", "app:build"], {
      cwd: root,
      encoding: "utf8",
      env: { ...quiet, QK_STORE_SERVER_TOKEN: token, QK_PROFILE_CACHE: "1", ...env },
    });
  const cache = () => spawnSync(qk, ["cache", "path"], { cwd: root, encoding: "utf8" }).stdout.trim();
  const received = (output) => {
    const bytes = output.stderr.match(/stage=remote_body \S+ bytes=(\d+)/)?.[1];
    if (bytes === undefined) throw new Error(`no remote_body span in qk's output:\n${output.stderr}`);
    return Number(bytes);
  };
  const restored = () => readFileSync(join(root, "app/dist/large.bin")).equals(readFileSync(join(root, "app/large.bin")));

  for (const [name, env] of [
    ["inline", {}],
    ["stored apart", { QK_REMOTE_BLOB_THRESHOLD: "64KB" }],
  ]) {
    writeFileSync(join(root, "app/variant.txt"), name);
    git("add", "-A");
    const built = run(write, env);
    check(built.status === 0 && built.stderr.includes("cache miss app:build"), `${name}: builds and uploads`);
    check(!built.stderr.includes("upload failed"), `${name}: the upload succeeds`);

    rmSync(cache(), { recursive: true, force: true });
    rmSync(join(root, "app/dist"), { recursive: true, force: true });
    const cold = run(read, env);
    check(cold.stderr.includes("remote cache hit app:build") && restored(), `${name}: a cold machine restores it`);

    rmSync(join(cache(), "entries"), { recursive: true, force: true });
    rmSync(join(root, "app/dist"), { recursive: true, force: true });
    const held = run(read, env);
    check(held.stderr.includes("remote cache hit app:build") && restored(), `${name}: a machine holding the files restores it`);
    check(received(held) < 100_000, `${name}: and receives ${received(held)} bytes, not the files it holds`);
  }

  writeFileSync(join(root, "app/variant.txt"), "read only");
  git("add", "-A");
  const refused = run(read);
  check(refused.stderr.includes("read-only"), "a read token cannot upload");

  const index = await (await fetch(`${url}/v4/index`, { headers: { Authorization: `Bearer ${read}` } })).text();
  check(/^run /m.test(index), "runs reach the server's index");
} catch (error) {
  failed = true;
  console.error(error);
} finally {
  wrangler.kill();
  rmSync(root, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
