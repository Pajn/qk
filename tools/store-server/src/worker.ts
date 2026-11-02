// The qk store server: the requests in PROTOCOL.md, over an R2 bucket and one
// Durable Object for the index.
//
// R2 holds `entries/<key>`, `warm/<a>/<b>`, `pathsets/<weak>` and
// `expectations/<name>` as the upload bodies clients sent, `blobs/<hash>` as one zstd frame each, and `sessions/<id>` as the
// Bloom filters sessions were opened with. A fetched record is composed from
// its upload body without touching the compressed bytes.

import { access } from "./auth";
import { Bloom, LARGEST_FILTER } from "./bloom";
import {
  APART,
  ByteReader,
  INLINE,
  LARGEST_HEADER,
  Malformed,
  ZSTD_MAGIC,
  concat,
  isHash,
  packHeader,
  parseHeader,
  parseUpload,
  rawFrame,
  utf8,
  type UploadHeader,
} from "./frames";
import type { Index } from "./index-do";

export { Index } from "./index-do";

export interface Env {
  STORE: R2Bucket;
  INDEX: DurableObjectNamespace<Index>;
  READ_TOKENS?: string;
  WRITE_TOKENS?: string;
}

/** The largest upload body, Bloom-free request or blob read into memory. */
const LARGEST_BODY = 100 << 20;
/** The largest blob a client uploads, as qk bounds it. */
const LARGEST_BLOB = 5 * 2 ** 30;
/** How many compressed bytes of blobs stored apart one pack carries inline. */
const INLINE_APART_BUDGET = 64 << 20;
const LARGEST_MISSING_QUERY = 4096;
const LARGEST_LISTING = 32 << 20;
/** The largest upload of a weak key's path sets, which cite no blob. */
const LARGEST_PATH_SETS = 16 << 20;
/** The largest upload of the tasks' expectations, which cite no blob. */
const LARGEST_EXPECTATIONS = 4 << 20;
const SESSION_LIFETIME = 24 * 60 * 60 * 1000;
/** How many R2 requests one request makes at a time. */
const CONCURRENCY = 32;
/** Blobs stored apart that a pack carries inline, read ahead of the one it is
 * sending, at most. */
const READ_AHEAD = 16;
/** Bytes every pack an isolate is sending may hold read ahead together. A
 * blob that does not fit is read on its turn and streamed, as one larger than
 * this always is. */
const READ_AHEAD_BYTES = 16 << 20;
let readAheadHeld = 0;
/** Where the edge cache files immutable objects; never fetched. */
const CACHE_ORIGIN = "https://qk-store-server.cache";

const KEY = /^[0-9a-f]{64}$/;
const WARM = /^[0-9a-f]{32}\/[0-9a-f]{32}$/;
const SESSION = /^[0-9a-f]{32}$/;
const EXPECTATIONS = /^[0-9a-f]{32}$/;

class Failure extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly extra: Record<string, unknown> = {},
  ) {
    super(message);
  }
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    try {
      return await handle(request, env, ctx);
    } catch (error) {
      if (error instanceof Failure) {
        return failure(error);
      }
      if (error instanceof Malformed) {
        return failure(new Failure(400, "malformed", error.message));
      }
      console.error(error);
      return failure(new Failure(503, "busy", "the store could not answer; try again"));
    }
  },
} satisfies ExportedHandler<Env>;

async function handle(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
  const url = new URL(request.url);
  const at = url.pathname.indexOf("/v4/");
  if (at < 0) {
    throw new Failure(404, "not_found", "no such request");
  }
  const path = url.pathname.slice(at + 4);
  const method = request.method;
  const granted = await access(request, env);
  if (!granted) {
    throw new Failure(401, "unauthorized", "the token is missing or unknown");
  }
  const write = () => {
    if (granted !== "write") {
      throw new Failure(403, "read_only", "a read token cannot write");
    }
  };
  const [kind, ...rest] = path.split("/");
  const name = rest.join("/");

  if (path === "sessions" && method === "POST") {
    return openSession(request, env);
  }
  if (path === "blobs/missing" && method === "POST") {
    return missingBlobs(request, env);
  }
  if (path === "index" && method === "GET") {
    return listIndex(url, env);
  }
  if (path === "runs" && method === "POST") {
    write();
    return recordRuns(request, env);
  }
  if (kind === "blobs" && isHash(name)) {
    if (method === "GET") {
      return getBlob(name, url, env, ctx);
    }
    if (method === "HEAD") {
      return (await env.STORE.head(`blobs/${name}`)) ? new Response(null) : notFound();
    }
    if (method === "PUT") {
      write();
      return putBlob(name, request, env);
    }
  }
  if (kind === "entries" && KEY.test(name)) {
    if (method === "GET") {
      return getPack(`entries/${name}`, true, url, env, ctx);
    }
    if (method === "HEAD") {
      return (await env.STORE.head(`entries/${name}`)) ? new Response(null) : notFound();
    }
    if (method === "PUT") {
      write();
      return putEntry(name, request, env);
    }
  }
  if (kind === "warm" && WARM.test(name)) {
    if (method === "GET") {
      return getPack(`warm/${name}`, false, url, env, ctx);
    }
    if (method === "HEAD") {
      return (await env.STORE.head(`warm/${name}`)) ? new Response(null) : notFound();
    }
    if (method === "PUT") {
      write();
      return putWarm(name, request, env);
    }
  }
  if (kind === "pathsets" && KEY.test(name)) {
    if (method === "GET") {
      return getPack(`pathsets/${name}`, false, url, env, ctx);
    }
    if (method === "HEAD") {
      return (await env.STORE.head(`pathsets/${name}`)) ? new Response(null) : notFound();
    }
    if (method === "PUT") {
      write();
      return putPathSets(name, request, env);
    }
  }
  if (kind === "expectations" && EXPECTATIONS.test(name)) {
    if (method === "GET") {
      return getPack(`expectations/${name}`, false, url, env, ctx);
    }
    if (method === "HEAD") {
      return (await env.STORE.head(`expectations/${name}`)) ? new Response(null) : notFound();
    }
    if (method === "PUT") {
      write();
      return putExpectations(name, request, env);
    }
  }
  throw new Failure(404, "not_found", "no such request");
}

// Sessions.

async function openSession(request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_FILTER);
  const bloom = Bloom.parse(body);
  if (!bloom) {
    throw new Failure(400, "malformed", "the body is not a version 1 Bloom filter");
  }
  const id = [...crypto.getRandomValues(new Uint8Array(16))].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  const expires = Date.now() + SESSION_LIFETIME;
  await env.STORE.put(`sessions/${id}`, body, {
    httpMetadata: { contentType: "application/x-qk-bloom" },
    customMetadata: { expires: String(expires) },
  });
  remember(id, bloom, expires);
  return Response.json({ session: id, expires }, { status: 201 });
}

/** Filters this isolate has loaded, oldest first, bounded in bytes. */
const sessions = new Map<string, { bloom: Bloom; expires: number }>();
const LARGEST_SESSION_CACHE = 16 << 20;
let sessionBytes = 0;

function remember(id: string, bloom: Bloom, expires: number): void {
  if (bloom.bits.length > LARGEST_SESSION_CACHE) {
    return;
  }
  sessions.set(id, { bloom, expires });
  sessionBytes += bloom.bits.length;
  for (const [oldest, { bloom: dropped }] of sessions) {
    if (sessionBytes <= LARGEST_SESSION_CACHE) {
      break;
    }
    sessions.delete(oldest);
    sessionBytes -= dropped.bits.length;
  }
}

/** The filter of session `id`, or `null` when there is no such live session:
 * a client that names none, or one that expired, is taken to hold nothing. */
async function session(id: string | null, env: Env): Promise<Bloom | null> {
  if (!id || !SESSION.test(id)) {
    return null;
  }
  const known = sessions.get(id);
  if (known) {
    return known.expires > Date.now() ? known.bloom : null;
  }
  const object = await env.STORE.get(`sessions/${id}`);
  if (!object) {
    return null;
  }
  const expires = Number(object.customMetadata?.expires);
  const bloom = Bloom.parse(new Uint8Array(await object.arrayBuffer()));
  if (!bloom || !(expires > Date.now())) {
    return null;
  }
  remember(id, bloom, expires);
  return bloom;
}

// Packs.

async function getPack(object: string, immutable: boolean, url: URL, env: Env, ctx: ExecutionContext): Promise<Response> {
  // A Worker's clock moves only across I/O, which is what these time.
  const started = Date.now();
  const held = await session(url.searchParams.get("session"), env);
  const sessioned = Date.now();
  const stored = immutable ? await readImmutable(object, env, ctx) : await env.STORE.get(object);
  const read = Date.now();
  if (!stored) {
    return notFound();
  }
  const reader = new ByteReader(stored.body);
  let body: ReadableStream<Uint8Array>;
  try {
    body = await compose(reader, held, env, ctx);
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  }
  return new Response(body, {
    headers: {
      "Content-Type": "application/x-qk-pack",
      "Cache-Control": "no-store",
      // Up to the first byte of the pack, which the rest streams after.
      "Server-Timing": `session;dur=${sessioned - started}, object;dur=${read - sessioned}, compose;dur=${Date.now() - read}`,
    },
  });
}

/** The pack of the upload body `reader` streams: a raw frame of the pack up to
 * its inline blobs, then each inline blob's zstd frame as it is stored. */
async function compose(
  reader: ByteReader,
  held: Bloom | null,
  env: Env,
  ctx: ExecutionContext,
): Promise<ReadableStream<Uint8Array>> {
  const headerLength = new DataView((await reader.read(4)).slice().buffer).getUint32(0, true);
  if (headerLength > LARGEST_HEADER) {
    throw new Error("a stored upload's header is too large");
  }
  const header = parseHeader(await reader.read(headerLength));
  const record = await reader.read(header.record);
  const { placements, sizes } = await place(header, held, env);
  const head = rawFrame(packHeader(record, header.blobs.map((blob, index) => ({ placement: placements[index]!, raw: blob.raw }))));
  // The stored frames past the last one this pack carries are never read.
  let lastStored = -1;
  header.blobs.forEach((blob, index) => {
    if (blob.compressed !== null && placements[index] === INLINE) {
      lastStored = index;
    }
  });

  // Read ahead in order, so a pack inlining many small blobs stored apart
  // waits on one R2 read at a time only when the reads cannot keep up.
  const apart = readAhead(
    header.blobs
      .filter((blob, index) => blob.compressed === null && placements[index] === INLINE)
      .map((blob) => ({ hash: blob.hash, size: sizes.get(blob.hash)! })),
    env,
    ctx,
  );

  async function* parts(): AsyncGenerator<Uint8Array> {
    try {
      yield head;
      for (const [index, blob] of header.blobs.entries()) {
        const inline = placements[index] === INLINE;
        if (blob.compressed !== null && index <= lastStored) {
          if (inline) {
            yield* reader.chunks(blob.compressed);
          } else {
            await reader.skip(blob.compressed);
          }
        } else if (blob.compressed === null && inline) {
          yield* await apart.next();
        }
      }
    } finally {
      apart.close();
      await reader.cancel().catch(() => {});
    }
  }
  return streamOf(parts());
}

/** Answers, in order, the bodies of the blobs stored apart `blobs` names,
 * each of the size the index has for it. Up to `READ_AHEAD` are read whole
 * ahead of the one asked for while they fit what the isolate's packs may hold
 * together; the rest are streamed on their turn. A body read ahead counts
 * against that until the pack asks for the next one, or `close`s; a read the
 * pack gave up counts until it settles. */
function readAhead(
  blobs: { hash: string; size: number }[],
  env: Env,
  ctx: ExecutionContext,
): { next: () => Promise<AsyncIterable<Uint8Array> | Iterable<Uint8Array>>; close: () => void } {
  let next = 0;
  const reading: { size: number; read: Promise<Uint8Array> }[] = [];
  // The body last answered, until the pack is past it.
  let sending = 0;
  const release = () => {
    readAheadHeld -= sending;
    sending = 0;
  };
  const fill = () => {
    while (reading.length < READ_AHEAD && next < blobs.length) {
      const { hash, size } = blobs[next]!;
      if (readAheadHeld + size > READ_AHEAD_BYTES) {
        return;
      }
      readAheadHeld += size;
      next++;
      const read = readImmutable(`blobs/${hash}`, env, ctx).then((stored) => {
        if (!stored) {
          throw new Error(`blob ${hash} went missing while a pack was sent`);
        }
        return readAtMost(stored.body, size, hash);
      });
      // Its failure is the pack's once it is that blob's turn, not before.
      read.catch(() => {});
      reading.push({ size, read });
    }
  };
  return {
    next: async () => {
      release();
      fill();
      const ahead = reading.shift();
      if (ahead) {
        sending = ahead.size;
        return [await ahead.read];
      }
      const blob = blobs[next++];
      if (!blob) {
        throw new Error("more blobs stored apart were asked for than the pack inlines");
      }
      const stored = await readImmutable(`blobs/${blob.hash}`, env, ctx);
      if (!stored) {
        throw new Error(`blob ${blob.hash} went missing while a pack was sent`);
      }
      return chunksOf(stored.body);
    },
    close: () => {
      release();
      for (const ahead of reading.splice(0)) {
        void ahead.read.then(
          () => {},
          () => {},
        ).finally(() => {
          readAheadHeld -= ahead.size;
        });
      }
    },
  };
}

/** The whole of `body`, which is to be `size` bytes: one that runs past it is
 * not the blob the index sized, as a write could have replaced it since. */
async function readAtMost(body: ReadableStream<Uint8Array>, size: number, hash: string): Promise<Uint8Array> {
  const bytes = new Uint8Array(size);
  let read = 0;
  for await (const chunk of chunksOf(body)) {
    if (read + chunk.length > size) {
      throw new Error(`blob ${hash} is larger than the index says`);
    }
    bytes.set(chunk, read);
    read += chunk.length;
  }
  return read === size ? bytes : bytes.subarray(0, read);
}

/** Where the pack of `header` carries each blob: apart when the session holds
 * it, inline when the upload carried it, and inline when it is stored apart
 * while those stay within the budget. */
async function place(
  header: UploadHeader,
  held: Bloom | null,
  env: Env,
): Promise<{ placements: number[]; sizes: Map<string, number | null> }> {
  const wanted = header.blobs.filter((blob) => blob.compressed === null && !held?.has(blob.hash));
  const sizes = await storedSizes(
    wanted.map((blob) => blob.hash),
    env,
  );
  let budget = 0;
  const placements = header.blobs.map((blob) => {
    if (held?.has(blob.hash)) {
      return APART;
    }
    if (blob.compressed !== null) {
      return INLINE;
    }
    const size = sizes.get(blob.hash);
    if (size == null || budget + size > INLINE_APART_BUDGET) {
      return APART;
    }
    budget += size;
    return INLINE;
  });
  return { placements, sizes };
}

// Blobs.

async function getBlob(hash: string, url: URL, env: Env, ctx: ExecutionContext): Promise<Response> {
  const entry = url.searchParams.get("entry");
  const warm = url.searchParams.get("warm");
  if ((entry !== null && !KEY.test(entry)) || (warm !== null && !WARM.test(warm)) || (entry !== null && warm !== null)) {
    throw new Failure(400, "malformed", "entry must be a key, or warm two 32-hex-digit names");
  }
  const zstd = { "Content-Type": "application/zstd" };
  const stored = await readImmutable(`blobs/${hash}`, env, ctx);
  if (stored) {
    return new Response(stored.body, { headers: { ...zstd, "Content-Length": String(stored.size) } });
  }
  const owner = entry !== null ? `entries/${entry}` : warm !== null ? `warm/${warm}` : null;
  const frame = owner ? await inlineFrame(owner, hash, env) : null;
  if (!frame) {
    return notFound();
  }
  return new Response(frame.body, { headers: { ...zstd, "Content-Length": String(frame.size) } });
}

/** The zstd frame of `hash` that the upload body stored as `object` carries
 * inline, read by range. */
async function inlineFrame(object: string, hash: string, env: Env): Promise<{ body: ReadableStream; size: number } | null> {
  const prefix = await env.STORE.get(object, { range: { offset: 0, length: 4 } });
  if (!prefix) {
    return null;
  }
  const headerLength = new DataView(await prefix.arrayBuffer()).getUint32(0, true);
  if (headerLength > LARGEST_HEADER) {
    throw new Error("a stored upload's header is too large");
  }
  const headerObject = await env.STORE.get(object, { range: { offset: 4, length: headerLength } });
  if (!headerObject) {
    return null;
  }
  const header = parseHeader(new Uint8Array(await headerObject.arrayBuffer()));
  let offset = 4 + headerLength + header.record;
  for (const blob of header.blobs) {
    if (blob.compressed === null) {
      continue;
    }
    if (blob.hash === hash) {
      const frame = await env.STORE.get(object, { range: { offset, length: blob.compressed } });
      return frame ? { body: frame.body, size: blob.compressed } : null;
    }
    offset += blob.compressed;
  }
  return null;
}

async function putBlob(hash: string, request: Request, env: Env): Promise<Response> {
  const key = `blobs/${hash}`;
  const declared = request.headers.get("Content-Length");
  let size: number;
  if (declared === null || !request.body) {
    // No length to stream with: read it whole.
    const body = await readBody(request, LARGEST_BODY);
    checkFrame(body);
    await env.STORE.put(key, body);
    size = body.length;
  } else {
    const length = Number(declared);
    size = length;
    if (!Number.isSafeInteger(length) || length < ZSTD_MAGIC.length) {
      throw new Failure(400, "malformed", "the body is not a zstd frame");
    }
    if (length > LARGEST_BLOB) {
      throw new Failure(413, "too_large", `a blob may be at most ${LARGEST_BLOB} bytes`);
    }
    // Check the start before storing anything, then stream the rest.
    const reader = request.body.getReader();
    const start: Uint8Array[] = [];
    let seen = 0;
    while (seen < ZSTD_MAGIC.length) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      start.push(value);
      seen += value.length;
    }
    try {
      checkFrame(concat(start));
    } catch (error) {
      await reader.cancel().catch(() => {});
      throw error;
    }
    const fixed = new FixedLengthStream(length);
    const writer = fixed.writable.getWriter();
    const copied = (async () => {
      try {
        for (const chunk of start) {
          await writer.write(chunk);
        }
        for (;;) {
          const { done, value } = await reader.read();
          if (done) {
            break;
          }
          await writer.write(value);
        }
        await writer.close();
      } catch (error) {
        await writer.abort(error).catch(() => {});
        throw error;
      }
    })();
    const [copy, put] = await Promise.allSettled([copied, env.STORE.put(key, fixed.readable)]);
    if (copy.status === "rejected") {
      throw new Failure(400, "malformed", "the body does not match its Content-Length");
    }
    if (put.status === "rejected") {
      throw put.reason;
    }
  }
  await indexOf(env).noteBlobs([[hash, size]]);
  await caches.default.delete(cacheUrl(key));
  return new Response(null, { status: 204 });
}

function checkFrame(body: Uint8Array): void {
  if (!ZSTD_MAGIC.every((byte, index) => body[index] === byte)) {
    throw new Malformed("the body is not a zstd frame");
  }
}

async function missingBlobs(request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, 1 << 20);
  let blobs: unknown;
  try {
    blobs = (JSON.parse(new TextDecoder().decode(body)) as { blobs?: unknown }).blobs;
  } catch {
    throw new Failure(400, "malformed", "the body is not JSON");
  }
  if (!Array.isArray(blobs) || blobs.length > LARGEST_MISSING_QUERY || !blobs.every(isHash)) {
    throw new Failure(400, "malformed", `blobs must be at most ${LARGEST_MISSING_QUERY} hashes`);
  }
  return Response.json({ missing: await lacking(blobs, env) });
}

/** Those of `hashes` not stored apart, in order. */
async function lacking(hashes: string[], env: Env): Promise<string[]> {
  const sizes = await storedSizes(hashes, env);
  return hashes.filter((hash) => sizes.get(hash) === null);
}

/** The stored size of each of `hashes` stored apart, or `null` for one that
 * is not, as the index has them. Until the index has listed the blobs stored
 * before it held them, one it lacks is asked of the bucket, a request each,
 * and noted when found. */
async function storedSizes(hashes: string[], env: Env): Promise<Map<string, number | null>> {
  const index = indexOf(env);
  const { sizes, complete } = await index.blobSizes(hashes);
  const known = new Map(hashes.map((hash, at) => [hash, sizes[at] ?? null]));
  if (complete) {
    return known;
  }
  const found: [string, number][] = [];
  await forEach(
    hashes.filter((hash) => known.get(hash) === null),
    async (hash) => {
      const object = await env.STORE.head(`blobs/${hash}`);
      if (object) {
        found.push([hash, object.size]);
        known.set(hash, object.size);
      }
    },
  );
  if (found.length > 0) {
    await index.noteFound(found);
  }
  return known;
}

function indexOf(env: Env): DurableObjectStub<Index> {
  return env.INDEX.get(env.INDEX.idFromName("index"));
}

// Commits.

async function putEntry(key: string, request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_BODY);
  const upload = parseUpload(body, key);
  const object = `entries/${key}`;
  const index = indexOf(env);
  const exists = async () => {
    // The entry is there, so this commit succeeded; list it in case the
    // first commit's listing was lost.
    await index.commit(key, Date.now());
    return failure(new Failure(409, "exists", "the entry was committed already"));
  };
  if (await env.STORE.head(object)) {
    return exists();
  }
  await requireBlobs(upload.header, env);
  const put = await env.STORE.put(object, body, {
    onlyIf: { etagDoesNotMatch: "*" },
    httpMetadata: { contentType: "application/x-qk-upload" },
  });
  if (!put) {
    return exists();
  }
  await index.commit(key, Date.now());
  return new Response(null, { status: 204 });
}

async function putWarm(name: string, request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_BODY);
  const upload = parseUpload(body);
  await requireBlobs(upload.header, env);
  await env.STORE.put(`warm/${name}`, body, { httpMetadata: { contentType: "application/x-qk-upload" } });
  return new Response(null, { status: 204 });
}

async function putPathSets(weak: string, request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_PATH_SETS);
  const upload = parseUpload(body);
  if (upload.header.blobs.length > 0) {
    throw new Failure(400, "malformed", "path sets cite no blob");
  }
  if (upload.json.weak !== weak) {
    throw new Failure(400, "malformed", `the record is not for weak key ${weak}`);
  }
  await env.STORE.put(`pathsets/${weak}`, body, { httpMetadata: { contentType: "application/x-qk-upload" } });
  return new Response(null, { status: 204 });
}

async function putExpectations(name: string, request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_EXPECTATIONS);
  const upload = parseUpload(body);
  if (upload.header.blobs.length > 0) {
    throw new Failure(400, "malformed", "expectations cite no blob");
  }
  if (upload.json.name !== name) {
    throw new Failure(400, "malformed", `the record is not named ${name}`);
  }
  await env.STORE.put(`expectations/${name}`, body, { httpMetadata: { contentType: "application/x-qk-upload" } });
  return new Response(null, { status: 204 });
}

/** Fails with `missing_blobs` unless every blob `header` stores apart is
 * stored. */
async function requireBlobs(header: UploadHeader, env: Env): Promise<void> {
  const apart = header.blobs.filter((blob) => blob.compressed === null).map((blob) => blob.hash);
  const missing = await lacking(apart, env);
  if (missing.length > 0) {
    throw new Failure(422, "missing_blobs", `the store lacks ${missing.length} blobs the record cites`, {
      blobs: missing,
    });
  }
}

// The index.

async function listIndex(url: URL, env: Env): Promise<Response> {
  const since = url.searchParams.get("since");
  if (since !== null && !/^\d{1,15}$/.test(since)) {
    throw new Failure(400, "malformed", "the cursor is not one this server gave");
  }
  const index = indexOf(env);
  const { text, cursor } = await index.list(since === null ? null : Number(since));
  return new Response(text, {
    headers: { "Content-Type": "text/plain; charset=utf-8", "Qk-Index-Cursor": cursor, "Cache-Control": "no-store" },
  });
}

async function recordRuns(request: Request, env: Env): Promise<Response> {
  const body = await readBody(request, LARGEST_LISTING);
  let text: string;
  try {
    text = utf8.decode(body);
  } catch {
    throw new Failure(400, "malformed", "the body is not UTF-8 text");
  }
  const index = indexOf(env);
  await index.record(text, Date.now());
  return new Response(null, { status: 204 });
}

// Reading.

/** The R2 object `key`, which never changes once written, through the edge
 * cache. */
async function readImmutable(
  key: string,
  env: Env,
  ctx: ExecutionContext,
): Promise<{ body: ReadableStream<Uint8Array>; size: number } | null> {
  const url = cacheUrl(key);
  const cached = await caches.default.match(url);
  if (cached?.body) {
    return { body: cached.body, size: Number(cached.headers.get("Content-Length")) };
  }
  const object = await env.STORE.get(key);
  if (!object) {
    return null;
  }
  const [body, copy] = object.body.tee();
  const headers = {
    "Content-Length": String(object.size),
    "Cache-Control": "public, max-age=31536000, immutable",
  };
  ctx.waitUntil(caches.default.put(url, new Response(copy, { headers })).catch(() => {}));
  return { body, size: object.size };
}

function cacheUrl(key: string): string {
  return `${CACHE_ORIGIN}/v4/${key}`;
}

/** The whole body of `request`, failing with `too_large` past `limit`. */
async function readBody(request: Request, limit: number): Promise<Uint8Array> {
  const tooLarge = () => new Failure(413, "too_large", `the body may be at most ${limit} bytes`);
  const declared = Number(request.headers.get("Content-Length") ?? NaN);
  if (declared > limit) {
    throw tooLarge();
  }
  if (!request.body) {
    return new Uint8Array(0);
  }
  if (Number.isSafeInteger(declared) && declared >= 0) {
    // One buffer of the declared length, rather than chunks and a copy.
    const out = new Uint8Array(declared);
    let length = 0;
    for await (const chunk of chunksOf(request.body)) {
      if (length + chunk.length > declared) {
        throw new Failure(400, "malformed", "the body is longer than its Content-Length");
      }
      out.set(chunk, length);
      length += chunk.length;
    }
    return out.subarray(0, length);
  }
  const parts: Uint8Array[] = [];
  let length = 0;
  for await (const chunk of chunksOf(request.body)) {
    length += chunk.length;
    if (length > limit) {
      throw tooLarge();
    }
    parts.push(chunk);
  }
  return concat(parts);
}

/** The chunks of `stream`. A consumer that stops before its end cancels the
 * rest, which the stream would otherwise keep producing for no one. */
async function* chunksOf(stream: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
  const reader = stream.getReader();
  let finished = false;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        finished = true;
        return;
      }
      yield value;
    }
  } finally {
    if (!finished) {
      await reader.cancel().catch(() => {});
    }
    reader.releaseLock();
  }
}

function streamOf(parts: AsyncGenerator<Uint8Array>): ReadableStream<Uint8Array> {
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      const { done, value } = await parts.next();
      if (done) {
        controller.close();
      } else {
        controller.enqueue(value);
      }
    },
    async cancel() {
      await parts.return(undefined);
    },
  });
}

/** Runs `task` on each of `items`, at most `CONCURRENCY` at a time. */
async function forEach<T>(items: T[], task: (item: T) => Promise<void>): Promise<void> {
  let next = 0;
  const workers = Array.from({ length: Math.min(CONCURRENCY, items.length) }, async () => {
    while (next < items.length) {
      await task(items[next++]!);
    }
  });
  await Promise.all(workers);
}

// Answers.

function notFound(): Response {
  return failure(new Failure(404, "not_found", "the object does not exist"));
}

function failure(error: Failure): Response {
  const headers: Record<string, string> = {};
  if (error.status === 401) {
    headers["WWW-Authenticate"] = "Bearer";
  }
  if (error.status === 429 || error.status === 503) {
    headers["Retry-After"] = "1";
  }
  return Response.json({ error: error.code, message: error.message, ...error.extra }, { status: error.status, headers });
}
