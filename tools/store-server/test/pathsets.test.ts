import { env, runInDurableObject } from "cloudflare:test";
import { decompress } from "fzstd";
import { describe, expect, it } from "vitest";
import { type Index, SAVING_AT_ONCE } from "../src/index-do";
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

  it("are answered by the index up to what one read of it carries, and as apart past that", async () => {
    const names = Array.from({ length: 24 }, () => `pathsets/${hash()}`);
    const size = 900 << 10;
    const states = await runInDurableObject(index(), (instance, state) => {
      for (const [at, name] of names.entries()) {
        state.storage.sql.exec(
          "INSERT INTO records (name, body, version, used) VALUES (?, ?, ?, ?)",
          name,
          new Uint8Array(size).buffer,
          1_000_000 + at,
          Date.now(),
        );
      }
      return (instance as unknown as Index).readRecords(names).map(({ state }) => state);
    });
    // Every record is held; the bodies of those past 16 MiB stay unread.
    const held = Math.floor((16 << 20) / size);
    expect(states).toEqual(names.map((_, at) => (at < held ? "held" : "apart")));
  });

  it("are the same in the index and the bucket after saves of one name at once", async () => {
    const weak = Array.from({ length: 8 }, hash);
    const records = (key: string) => ["a", "b", "c"].map((read) => upload(pathSets(key, read), []));
    const saved = await Promise.all(
      weak.flatMap((key) => records(key).map((body) => request("PUT", `pathsets/${key}`, { body }))),
    );
    expect(saved.map((response) => response.status)).toEqual(saved.map(() => 204));

    for (const key of weak) {
      const name = `pathsets/${key}`;
      const kept = new Uint8Array(await (await env.STORE.get(name))!.arrayBuffer());
      expect(records(key).some((body) => hex(body) === hex(kept))).toBe(true);
      const [held] = await runInDurableObject(index(), (instance) => (instance as unknown as Index).readRecords([name]));
      expect(held).toEqual({ state: "held", body: kept });
    }
  });

  it("are read from the bucket, and not taken in, while a save is on its way", async () => {
    const weak = hash();
    const name = `pathsets/${weak}`;
    await save(weak, pathSets(weak, "before"));
    // As a save leaves the index between its two steps, or for good when it
    // never ends: without a body, and the bucket already written.
    const record = pathSets(weak, "written");
    await env.STORE.put(name, upload(record, []));
    await runInDurableObject(index(), (_, state) => {
      state.storage.sql.exec("UPDATE records SET body = NULL, version = version + 1, saving = 1 WHERE name = ?", name);
    });
    const expected = expectedPack(record, [], () => true);

    expect(await getOne(weak)).toEqual(expected);
    expect(await fetchMany([weak])).toEqual([[weak, expected]]);
    const [state] = await runInDurableObject(index(), (instance) =>
      (instance as unknown as Index).readRecords([name]).map(({ state }) => state),
    );
    expect(state).toBe("apart");
    // The next save ends it.
    await save(weak, pathSets(weak, "after"));
    await env.STORE.delete(name);
    expect(await getOne(weak)).toEqual(expectedPack(pathSets(weak, "after"), [], () => true));
  });

  it("are refused as busy while the index is saving as much as it takes at once", async () => {
    const weak = hash();
    const record = upload(pathSets(weak, "waiting"), []);
    const tickets = await runInDurableObject(index(), async (instance) => {
      const stub = instance as unknown as Index;
      // One save is admitted whatever its size; more only within the bound.
      const large = stub.admitSave(SAVING_AT_ONCE);
      expect(large).not.toBeNull();
      expect(stub.admitSave(1)).toBeNull();
      // A ticket saves once, and only a body it was given for.
      expect(await stub.saveRecord(`pathsets/${hash()}`, record, "no ticket")).toBe(false);
      return [large!];
    });
    const refused = await request("PUT", `pathsets/${weak}`, { body: record });
    expect(refused.status).toBe(503);
    expect(((await refused.json()) as { error: string }).error).toBe("busy");
    expect(await env.STORE.head(`pathsets/${weak}`)).toBeNull();

    await runInDurableObject(index(), async (instance) => {
      const stub = instance as unknown as Index;
      const small = new Uint8Array(8);
      // Room is given back when its save ends.
      expect(await stub.saveRecord(`pathsets/${hash()}`, small, tickets[0]!)).toBe(true);
      expect(await stub.saveRecord(`pathsets/${hash()}`, small, tickets[0]!)).toBe(false);
      const ticket = stub.admitSave(4);
      expect(await stub.saveRecord(`pathsets/${hash()}`, small, ticket!)).toBe(false);
    });
    expect((await request("PUT", `pathsets/${weak}`, { body: record })).status).toBe(204);
  });

  it("are never taken in from a read older than a save the index heard of", async () => {
    const weak = hash();
    const name = `pathsets/${weak}`;
    const older = upload(pathSets(weak, "older"), []);
    const newer = upload(pathSets(weak, "newer"), []);
    await runInDurableObject(index(), async (instance, state) => {
      const stub = instance as unknown as Index;
      // Listed from the bucket without its body, as the read began.
      state.storage.sql.exec("INSERT INTO records (name, body, version, used) VALUES (?, NULL, 1, ?)", name, Date.now());
      const [began] = stub.readRecords([name]);
      expect(began!.state).toBe("apart");
      // A save, then the slow read of the older body ends.
      expect(await stub.saveRecord(name, newer, stub.admitSave(newer.length)!)).toBe(true);
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
