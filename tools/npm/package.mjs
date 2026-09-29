#!/usr/bin/env node
// Assembles the npm packages for a release from prebuilt binaries, the way
// oxlint and biome ship: a main package whose `qk` bin launches the binary from
// the optional dependency matching the platform, and one package per platform.
//
//   node tools/npm/package.mjs --name <package> --version <version> --binaries <dir> --out <dir>
//
// <binaries> holds one directory per Rust target, each with the qk binary
// (qk.exe on Windows). Scoped names work: @scope/qk gets @scope/qk-linux-x64
// and so on. The packages are written to <out>/<package directory>, ready for
// `npm publish` in each.

import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const args = process.argv.slice(2);
const option = (name) => {
  const index = args.indexOf(name);
  if (index < 0) throw new Error(`missing ${name}`);
  return args[index + 1];
};
const name = option("--name");
const version = option("--version");
const binaries = resolve(option("--binaries"));
const out = resolve(option("--out"));

/// Rust targets and the npm os and cpu values they serve.
const PLATFORMS = [
  { target: "x86_64-unknown-linux-gnu", os: "linux", cpu: "x64", binary: "qk" },
  { target: "aarch64-unknown-linux-gnu", os: "linux", cpu: "arm64", binary: "qk" },
  { target: "aarch64-apple-darwin", os: "darwin", cpu: "arm64", binary: "qk" },
  { target: "x86_64-pc-windows-msvc", os: "win32", cpu: "x64", binary: "qk.exe" },
];

const repository = JSON.parse(readFileSync(new URL("./repository.json", import.meta.url), "utf8"));
const directory = (packageName) => packageName.replace(/^@/, "").replace("/", "__");
// License and repository are added once repository.json names them.
const common = {
  version,
  ...(repository.license && { license: repository.license }),
  ...(repository.repository && { repository: repository.repository }),
};

const optionalDependencies = {};
for (const platform of PLATFORMS) {
  const packageName = `${name}-${platform.os}-${platform.cpu}`;
  const source = join(binaries, platform.target, platform.binary);
  if (!existsSync(source)) throw new Error(`no binary for ${platform.target} at ${source}`);
  const target = join(out, directory(packageName));
  mkdirSync(join(target, "bin"), { recursive: true });
  copyFileSync(source, join(target, "bin", platform.binary));
  chmodSync(join(target, "bin", platform.binary), 0o755);
  writeFileSync(
    join(target, "package.json"),
    `${JSON.stringify(
      {
        name: packageName,
        ...common,
        description: `The qk binary for ${platform.os} ${platform.cpu}.`,
        os: [platform.os],
        cpu: [platform.cpu],
        files: ["bin"],
        preferUnplugged: true,
      },
      null,
      2,
    )}\n`,
  );
  optionalDependencies[packageName] = version;
}

const main = join(out, directory(name));
mkdirSync(join(main, "bin"), { recursive: true });
writeFileSync(
  join(main, "package.json"),
  `${JSON.stringify(
    {
      name,
      ...common,
      description: repository.description,
      bin: { qk: "bin/qk.js" },
      files: ["bin"],
      engines: { node: ">=18" },
      optionalDependencies,
    },
    null,
    2,
  )}\n`,
);
writeFileSync(
  join(main, "bin", "qk.js"),
  `#!/usr/bin/env node
// Runs the qk binary from the optional dependency for this platform.
const { spawnSync } = require("node:child_process");
const platform = \`\${process.platform}-\${process.arch}\`;
const binary = process.platform === "win32" ? "qk.exe" : "qk";
let path;
try {
  path = require.resolve(\`${name}-\${platform}/bin/\${binary}\`);
} catch {
  console.error(\`qk: no prebuilt binary for \${platform}; supported: ${PLATFORMS.map((platform) => `${platform.os}-${platform.cpu}`).join(", ")}\`);
  process.exit(1);
}
const result = spawnSync(path, process.argv.slice(2), { stdio: "inherit" });
if (result.error) throw result.error;
if (result.signal) process.kill(process.pid, result.signal);
process.exit(result.status ?? 1);
`,
);
chmodSync(join(main, "bin", "qk.js"), 0o755);
console.log(`wrote ${PLATFORMS.length + 1} packages for ${name}@${version} to ${out}`);
