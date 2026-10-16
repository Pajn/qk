import { describe, expect, it } from "vitest";
import { READ, hash, record, request, upload } from "./helpers";

async function commit(key: string): Promise<void> {
  const response = await request("PUT", `entries/${key}`, { body: upload(record(key, []), []) });
  expect(response.status).toBe(204);
}

async function index(since?: string): Promise<{ lines: string[]; cursor: string }> {
  const response = await request("GET", since === undefined ? "index" : `index?since=${since}`, { token: READ });
  expect(response.status).toBe(200);
  const cursor = response.headers.get("Qk-Index-Cursor");
  expect(cursor).not.toBeNull();
  return { lines: (await response.text()).split("\n").filter(Boolean), cursor: cursor! };
}

const keyLines = (lines: string[]) => lines.filter((line) => !line.startsWith("run "));
const runLines = (lines: string[]) => lines.filter((line) => line.startsWith("run "));

describe("the index", () => {
  it("lists committed keys, and after a cursor only the newer ones", async () => {
    const [first, second, third] = [hash(), hash(), hash()];
    await commit(first);
    await commit(second);
    const all = await index();
    const keys = keyLines(all.lines).map((line) => line.split(" ")[0]);
    expect(keys).toEqual(expect.arrayContaining([first, second]));
    expect(keys).toEqual([...keys].sort());
    for (const line of keyLines(all.lines)) {
      expect(line).toMatch(/^[0-9a-f]{64} \d+$/);
    }
    await commit(third);
    const since = await index(all.cursor);
    expect(keyLines(since.lines).map((line) => line.split(" ")[0])).toEqual([third]);
    expect(Number(since.cursor)).toBeGreaterThan(Number(all.cursor));
  });

  it("remembers runs and renews the keys they found", async () => {
    const key = hash();
    await commit(key);
    const { cursor } = await index();
    const later = Date.now() + 1000;
    const listing = `${key} ${later}\n${hash()} ${later}\nnot a line\nrun run-a ${Date.now()} ${key.slice(0, 16)} zz\n`;
    expect((await request("POST", "runs", { body: listing })).status).toBe(204);
    const since = await index(cursor);
    // A key the index does not hold is not listed by a run alone.
    expect(keyLines(since.lines)).toEqual([`${key} ${later}`]);
    expect(runLines(since.lines)).toContain(`run run-a ${listing.split("run run-a ")[1]!.split(" ")[0]} ${key.slice(0, 16)}`);
  });

  it("keeps the 32 most recent runs", async () => {
    const now = Date.now();
    const listing = Array.from({ length: 40 }, (_, index) => `run trim-${String(index).padStart(2, "0")} ${now + index} ${hash().slice(0, 16)}`);
    expect((await request("POST", "runs", { body: listing.join("\n") })).status).toBe(204);
    const runs = runLines((await index()).lines);
    expect(runs).toHaveLength(32);
    expect(runs[0]).toMatch(/^run trim-39 /);
    expect(runs[31]).toMatch(/^run trim-08 /);
    const old = `run stale ${now - 15 * 24 * 60 * 60 * 1000} ${hash().slice(0, 16)}`;
    expect((await request("POST", "runs", { body: old })).status).toBe(204);
    expect(runLines((await index()).lines).some((line) => line.startsWith("run stale "))).toBe(false);
  });

  it("refuses a cursor it did not give", async () => {
    expect((await request("GET", "index?since=soon")).status).toBe(400);
  });
});
