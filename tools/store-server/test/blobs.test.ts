import { env, runInDurableObject } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import { rawFrame } from "../src/frames";
import { READ, blob, putApart, request } from "./helpers";

function index() {
  return env.INDEX.get(env.INDEX.idFromName("index"));
}

/** Stores `raw` as a blob straight into the bucket, as one stored before the
 * index held blobs was. */
async function storeBefore(text: string) {
  const old = blob(text, false);
  const frame = rawFrame(old.raw);
  await env.STORE.put(`blobs/${old.hash}`, frame);
  return { hash: old.hash, size: frame.length };
}

/** Makes the index one that has not yet listed the blobs stored before it
 * held them. The listing it schedules on its own is finished first, so that
 * it cannot complete behind the test's back. */
async function unlisted(): Promise<void> {
  while (!(await index().blobsStored([])).complete) {
    await runInDurableObject(index(), (instance) => instance.alarm!());
  }
  await runInDurableObject(index(), async (_, state) => {
    await state.storage.deleteAlarm();
    state.storage.sql.exec("DELETE FROM listing");
  });
}

async function missing(hashes: string[]): Promise<string[]> {
  const response = await request("POST", "blobs/missing", { token: READ, body: JSON.stringify({ blobs: hashes }) });
  expect(response.status).toBe(200);
  return (await response.json<{ missing: string[] }>()).missing;
}

describe("the index of blobs", () => {
  beforeEach(unlisted);

  it("holds each blob uploaded apart with its stored size", async () => {
    const uploaded = blob("uploaded apart", false);
    await putApart(uploaded);
    const { stored } = await index().blobsStored([uploaded.hash]);
    expect(stored.map((blob) => blob?.size)).toEqual([rawFrame(uploaded.raw).length]);
  });

  it("finds a blob stored before it held them while it lists the bucket, and keeps it", async () => {
    const old = await storeBefore("stored before the index");
    expect(await index().blobsStored([old.hash])).toEqual({ stored: [null], complete: false });
    expect(await missing([old.hash])).toEqual([]);
    expect((await index().blobsStored([old.hash])).stored).toEqual([{ size: old.size, bundle: null, offset: 0 }]);
  });

  it("keeps a size an upload noted over one found in the bucket", async () => {
    const noted = blob("noted by an upload", false);
    await index().noteBlobs([[noted.hash, 10]]);
    await index().noteFound([[noted.hash, 5]]);
    expect((await index().blobsStored([noted.hash])).stored).toEqual([{ size: 10, bundle: null, offset: 0 }]);
  });

  it("keeps a blob in the bundle it was first noted in, and moves a loose one into one", async () => {
    const shared = blob("in two bundles", false);
    await index().noteBundled("a".repeat(64), [[shared.hash, 10, 100]]);
    await index().noteBundled("b".repeat(64), [[shared.hash, 12, 200]]);
    expect((await index().blobsStored([shared.hash])).stored).toEqual([
      { size: 10, bundle: "a".repeat(64), offset: 100 },
    ]);
    // Nor does finding it stored on its own, as a listing of the bucket would.
    await index().noteFound([[shared.hash, 5]]);
    expect((await index().blobsStored([shared.hash])).stored).toEqual([
      { size: 10, bundle: "a".repeat(64), offset: 100 },
    ]);
    const loose = blob("stored on its own first", false);
    await index().noteBlobs([[loose.hash, 7]]);
    await index().noteBundled("c".repeat(64), [[loose.hash, 8, 300]]);
    expect((await index().blobsStored([loose.hash])).stored).toEqual([{ size: 8, bundle: "c".repeat(64), offset: 300 }]);
  });

  it("lists the bucket again later when a listing fails", async () => {
    await runInDurableObject(index(), async (instance, state) => {
      const held = instance as unknown as { env: { STORE: R2Bucket } };
      const store = held.env.STORE;
      held.env = { STORE: { list: () => Promise.reject(new Error("unavailable")) } as unknown as R2Bucket };
      try {
        await instance.alarm!();
      } finally {
        held.env = { STORE: store };
      }
      expect(await state.storage.getAlarm()).toBeGreaterThan(Date.now());
    });
    expect((await index().blobsStored([])).complete).toBe(false);
  });

  it("lists the blobs stored before it held them, once", async () => {
    const old = await storeBefore("listed from the bucket");
    await runInDurableObject(index(), (instance) => instance.alarm!());
    expect(await index().blobsStored([old.hash])).toEqual({
      stored: [{ size: old.size, bundle: null, offset: 0 }],
      complete: true,
    });
  });
});
