// Records that a task ran, and fails it when its project holds a FAIL file.
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [id, projectRoot = "."] = process.argv.slice(2);
const root = process.env.FIXTURE_ROOT ?? process.cwd();
mkdirSync(join(root, ".ran"), { recursive: true });
writeFileSync(join(root, ".ran", id.replaceAll(/[:/@]/g, "_")), id);
if (existsSync(join(projectRoot, "FAIL"))) {
  console.error(`${id} failed`);
  process.exit(1);
}
console.log(`${id} ran`);
