import { env, runInDurableObject } from "cloudflare:test";
import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import type { Index } from "../src/index-do";
import { READ, bytes, expectedPack, hash, hex, request, sha256, upload } from "./helpers";

const pathSets = (weak: string, read: string, padding = 0) =>
  bytes(JSON.stringify({ version: 1, weak, sets: [{ reads: [read] }], padding: "x".repeat(padding) }));

async function save(weak: string, record: Uint8Array): Promise<void> {
  const response = await request("PUT", `pathsets/${weak}`, { body: upload(record, []) });
  expect(response.status, await response.clone().text()).toBe(204);
}

async function getOne(weak: string): Promise<Uint8Array | null> {
  const response = await request("GET", `pathsets/${weak}`, { token: READ });
  if (response.status === 404) {
    await response.arrayBuffer();
    return null;
  }
  expect(response.status).toBe(200);
  return decompress(new Uint8Array(await response.arrayBuffer()));
}

/** Each weak key a `POST pathsets/fetch` answer lists, with its decoded pack
 * or `null` for none, in the order listed. */
async function fetchMany(weak: string[]): Promise<[string, Uint8Array | null][]> {
  const response = await request("POST", "pathsets/fetch", { token: READ, body: JSON.stringify({ weak }) });
  if (response.status !== 200) {
    throw new Error(`POST pathsets/fetch: ${response.status} ${await response.text()}`);
  }
  expect(response.headers.get("Content-Type")).toBe("application/x-qk-path-sets");
  expect(response.headers.get("Server-Timing")).toMatch(/^index;dur=\d+, object;dur=-?\d+, compose;dur=\d+$/);
  const body = new Uint8Array(await response.arrayBuffer());
  const view = new DataView(body.buffer, body.byteOffset);
  expect(new TextDecoder().decode(body.subarray(0, 4))).toBe("qks1");
  const count = view.getUint32(4, true);
  let at = 8 + count * 40;
  const listed: [string, Uint8Array | null][] = [];
  for (let index = 0; index < count; index++) {
    const entry = 8 + index * 40;
    const size = Number(view.getBigUint64(entry + 32, true));
    listed.push([hex(body.subarray(entry, entry + 32)), size === 0 ? null : decompress(body.subarray(at, at + size))]);
    at += size;
  }
  expect(at).toBe(body.length);
  return listed;
}

function index() {
  return env.INDEX.get(env.INDEX.idFromName("index"));
}

describe("POST pathsets/fetch", () => {
  it("answers each weak key asked for once, with its pack or none, to a read token", async () => {
    const [first, second, absent] = [hash(), hash(), hash()];
    await save(first, pathSets(first, "a"));
    await save(second, pathSets(second, "b"));

    const listed = await fetchMany([first, absent, second, first]);

    expect(listed).toEqual([
      [first, expectedPack(pathSets(first, "a"), [], () => true)],
      [absent, null],
      [second, expectedPack(pathSets(second, "b"), [], () => true)],
    ]);
  });

  it("answers the newest save, as GET pathsets/<weak> does", async () => {
    const weak = hash();
    await save(weak, pathSets(weak, "old"));
    expect(await getOne(weak)).toEqual(expectedPack(pathSets(weak, "old"), [], () => true));
    expect(await fetchMany([weak])).toEqual([[weak, expectedPack(pathSets(weak, "old"), [], () => true)]]);
    await save(weak, pathSets(weak, "new"));
    expect(await getOne(weak)).toEqual(expectedPack(pathSets(weak, "new"), [], () => true));
    expect(await fetchMany([weak])).toEqual([[weak, expectedPack(pathSets(weak, "new"), [], () => true)]]);
  });

  it("reads from the bucket a record too large for the index to hold", async () => {
    const weak = hash();
    const large = pathSets(weak, "large", 3 << 20);
    await save(weak, large);
    // Compared by digest: arrays this size compare slowly.
    const expected = await sha256(expectedPack(large, [], () => true));
    expect(await sha256((await getOne(weak))!)).toBe(expected);
    const [[listed, pack]] = (await fetchMany([weak])) as [[string, Uint8Array]];
    expect(listed).toBe(weak);
    expect(await sha256(pack)).toBe(expected);
    // A smaller save after it is held, and answered, instead.
    await save(weak, pathSets(weak, "small"));
    expect(await fetchMany([weak])).toEqual([[weak, expectedPack(pathSets(weak, "small"), [], () => true)]]);
  });

  it("leaves out what would take its packs past 32 MiB, so the client asks again", async () => {
    const weak = Array.from({ length: 3 }, hash);
    for (const [at, key] of weak.entries()) {
      await save(key, pathSets(key, String(at), 15 << 20));
    }
    const first = await fetchMany(weak);
    expect(first.length).toBe(2);
    const left = weak.filter((key) => !first.some(([listed]) => listed === key));
    expect(left.length).toBe(1);
    expect((await fetchMany(left)).map(([listed]) => listed)).toEqual(left);
  });

  it("refuses more than 4,096 weak keys, or what is not one", async () => {
    const tooMany = JSON.stringify({ weak: Array.from({ length: 4097 }, hash) });
    expect((await request("POST", "pathsets/fetch", { token: READ, body: tooMany })).status).toBe(400);
    const notKeys = JSON.stringify({ weak: ["not a weak key"] });
    expect((await request("POST", "pathsets/fetch", { token: READ, body: notKeys })).status).toBe(400);
    expect((await request("POST", "pathsets/fetch", { token: null, body: "{}" })).status).toBe(401);
  });
});

describe("records citing no blob", () => {
  it("saved before the index held them are read from the bucket until it lists them", async () => {
    const weak = hash();
    const name = `pathsets/${weak}`;
    const record = pathSets(weak, "older");
    // As a server from before the index held records left it: in the bucket
    // alone, and the listing of path sets not yet done.
    await runInDurableObject(index(), (_, state) => {
      state.storage.sql.exec("DELETE FROM listing WHERE name LIKE 'pathsets%'");
    });
    await env.STORE.put(name, upload(record, []));
    const expected = expectedPack(record, [], () => true);

    expect(await getOne(weak)).toEqual(expected);
    expect(await fetchMany([weak])).toEqual([[weak, expected]]);
    // Taken in as it was read, so the index answers it from now on.
    await env.STORE.delete(name);
    expect(await fetchMany([weak])).toEqual([[weak, expected]]);
  });

  it("saved before the index held them are listed once, and one it lacks then is none", async () => {
    const weak = hash();
    const name = `pathsets/${weak}`;
    const record = pathSets(weak, "listed");
    await env.STORE.put(name, upload(record, []));
    await runInDurableObject(index(), async (instance, state) => {
      const stub = instance as unknown as Index;
      state.storage.sql.exec("DELETE FROM listing WHERE name LIKE 'pathsets%'");
      expect(stub.readRecords([name, `pathsets/${hash()}`]).map(({ state }) => state)).toEqual(["unknown", "unknown"]);
      await stub.alarm();
      while ((await state.storage.getAlarm()) !== null) {
        await state.storage.deleteAlarm();
        await stub.alarm();
      }
      expect(stub.readRecords([name, `pathsets/${hash()}`]).map(({ state }) => state)).toEqual(["apart", "none"]);
    });
    expect(await fetchMany([weak])).toEqual([[weak, expectedPack(record, [], () => true)]]);
  });

  it("are never taken in from a read older than a save the index heard of", async () => {
    const weak = hash();
    const name = `pathsets/${weak}`;
    const older = upload(pathSets(weak, "older"), []);
    const newer = upload(pathSets(weak, "newer"), []);
    await runInDurableObject(index(), (instance) => {
      const stub = instance as unknown as Index;
      // Listed from the bucket without its body, as the read began.
      stub.saveRecord(name, null);
      const [began] = stub.readRecords([name]);
      expect(began!.state).toBe("apart");
      // A save, then the slow read of the older body ends.
      stub.saveRecord(name, newer);
      stub.fillRecord(name, (began as { version: number }).version, older);
      expect(stub.readRecords([name])).toEqual([{ state: "held", body: newer }]);
    });
  });

  it("serve expectations from the index too, without reading a session", async () => {
    const name = hash().slice(0, 32);
    const record = bytes(JSON.stringify({ version: 1, name, tasks: {} }));
    expect((await request("PUT", `expectations/${name}`, { body: upload(record, []) })).status).toBe(204);
    await env.STORE.delete(`expectations/${name}`);
    const response = await request("GET", `expectations/${name}?session=${hash().slice(0, 32)}`, { token: READ });
    expect(response.status).toBe(200);
    expect(response.headers.get("Server-Timing")).toMatch(/^session;dur=0, /);
    expect(decompress(new Uint8Array(await response.arrayBuffer()))).toEqual(expectedPack(record, [], () => true));
  });
});
