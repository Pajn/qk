import { describe, expect, it } from "vitest";
import { rawFrame } from "../src/frames";
import { READ, blob, hash, hex, putApart, putBundle, request } from "./helpers";

/** Each blob a `POST blobs/fetch` answer lists, by hash, with its frame, in
 * the order listed. */
async function fetchBlobs(hashes: string[]): Promise<[string, Uint8Array][]> {
  const response = await request("POST", "blobs/fetch", { token: READ, body: JSON.stringify({ blobs: hashes }) });
  if (response.status !== 200) {
    throw new Error(`POST blobs/fetch: ${response.status} ${await response.text()}`);
  }
  expect(response.headers.get("Content-Type")).toBe("application/x-qk-blobs");
  const body = new Uint8Array(await response.arrayBuffer());
  const view = new DataView(body.buffer, body.byteOffset);
  expect(new TextDecoder().decode(body.subarray(0, 4))).toBe("qkb1");
  const count = view.getUint32(4, true);
  let at = 8 + count * 40;
  const listed: [string, Uint8Array][] = [];
  for (let index = 0; index < count; index++) {
    const entry = 8 + index * 40;
    const size = Number(view.getBigUint64(entry + 32, true));
    listed.push([hex(body.subarray(entry, entry + 32)), body.subarray(at, at + size)]);
    at += size;
  }
  expect(at).toBe(body.length);
  return listed;
}

describe("POST blobs/fetch", () => {
  it("answers bundled and loose blobs with the frames GET blobs/<hash> answers, each once", async () => {
    const bundled = [blob("first bundled", false), blob("second bundled", false), blob("third bundled", false)];
    await putBundle(bundled);
    const loose = blob("stored on its own", false);
    await putApart(loose);
    const unknown = hash();
    const asked = [loose.hash, unknown, ...bundled.map((blob) => blob.hash), loose.hash];

    const listed = await fetchBlobs(asked);

    const frames = new Map(listed);
    expect(listed).toHaveLength(4);
    expect(frames.has(unknown)).toBe(false);
    for (const stored of [...bundled, loose]) {
      expect(frames.get(stored.hash)).toEqual(rawFrame(stored.raw));
    }
    // A bundle's blobs first, in the order they lie in it, then the loose one.
    expect(listed.at(-1)![0]).toBe(loose.hash);
  });

  it("leaves out what would take its frames past 64 MiB, so the client asks again", async () => {
    const large = [0, 1].map((fill) => ({ hash: hash(), raw: new Uint8Array(40 << 20).fill(fill), inline: false }));
    for (const stored of large) {
      await putApart(stored);
    }
    const small = blob("fits beside one large blob", false);
    await putApart(small);

    const first = await fetchBlobs([large[0]!.hash, large[1]!.hash, small.hash]);

    expect(first.map(([hash]) => hash).sort()).toEqual([large[0]!.hash, small.hash].sort());
    const second = await fetchBlobs([large[1]!.hash]);
    expect(second.map(([hash]) => hash)).toEqual([large[1]!.hash]);
  });

  it("answers an empty list for blobs it does not store apart", async () => {
    expect(await fetchBlobs([hash()])).toEqual([]);
  });

  it("refuses more than 4,096 blobs", async () => {
    const asked = Array.from({ length: 4097 }, hash);
    const response = await request("POST", "blobs/fetch", { token: READ, body: JSON.stringify({ blobs: asked }) });
    expect(response.status).toBe(400);
  });
});
