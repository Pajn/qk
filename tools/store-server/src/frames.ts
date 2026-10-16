// The byte formats in PROTOCOL.md: raw zstd frames, upload bodies, and the
// part of a v4 pack (crates/qk-cache/src/remote/pack.rs) up to its inline
// blobs. Nothing here compresses or decompresses: a pack is a raw frame of its
// header followed by the zstd frames the client uploaded, unchanged.

export const ZSTD_MAGIC = [0x28, 0xb5, 0x2f, 0xfd] as const;
const LARGEST_BLOCK = 128 << 10;
const PACK_MAGIC = new TextEncoder().encode("qkp4");
/** The largest upload header a server reads. */
export const LARGEST_HEADER = 16 << 20;
/** The largest record an upload carries, as the client bounds it. */
export const LARGEST_RECORD = 1 << 30;

/** A Zstandard frame of `data` in raw blocks: the magic, a frame header
 * descriptor with no flags, a 128 KiB window, then blocks of at most 128 KiB. */
export function rawFrame(data: Uint8Array): Uint8Array {
  const blocks = Math.max(1, Math.ceil(data.length / LARGEST_BLOCK));
  const frame = new Uint8Array(6 + blocks * 3 + data.length);
  frame.set(ZSTD_MAGIC, 0);
  frame[4] = 0x00;
  frame[5] = 0x38;
  let at = 6;
  for (let block = 0; block < blocks; block++) {
    const start = block * LARGEST_BLOCK;
    const size = Math.min(LARGEST_BLOCK, data.length - start);
    const last = block === blocks - 1 ? 1 : 0;
    // size << 3 | type (0, raw) << 1 | last, as 3 bytes little endian.
    const header = size * 8 + last;
    frame[at] = header & 0xff;
    frame[at + 1] = (header >>> 8) & 0xff;
    frame[at + 2] = (header >>> 16) & 0xff;
    frame.set(data.subarray(start, start + size), at + 3);
    at += 3 + size;
  }
  return frame;
}

/** A blob an upload names: its hash, its raw length, and the length of its
 * zstd frame in the upload, or `null` when it is stored apart. */
export interface UploadBlob {
  hash: string;
  raw: number;
  compressed: number | null;
}

/** An upload body's header. */
export interface UploadHeader {
  record: number;
  blobs: UploadBlob[];
}

/** An upload body, read whole. */
export interface Upload {
  header: UploadHeader;
  record: Uint8Array;
  /** The record, parsed. */
  json: Record<string, unknown>;
  /** Where each blob carried inline starts in the body, by hash. */
  inline: Map<string, { offset: number; length: number }>;
}

/** Why a body is not an upload. */
export class Malformed extends Error {}

const HASH = /^[0-9a-f]{64}$/;

/** Decodes UTF-8, failing on what is not. */
export const utf8 = new TextDecoder("utf-8", { fatal: true, ignoreBOM: false });

export function isHash(value: unknown): value is string {
  return typeof value === "string" && HASH.test(value);
}

function isLength(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/** The header an upload's JSON names. Throws `Malformed` unless the record
 * length is a length, and the blobs are sorted, unique hashes with lengths. */
export function parseHeader(bytes: Uint8Array): UploadHeader {
  let value: unknown;
  try {
    value = JSON.parse(utf8.decode(bytes));
  } catch {
    throw new Malformed("the upload header is not JSON");
  }
  if (typeof value !== "object" || value === null) {
    throw new Malformed("the upload header is not an object");
  }
  const { record, blobs } = value as { record?: unknown; blobs?: unknown };
  if (!isLength(record) || record > LARGEST_RECORD) {
    throw new Malformed("the upload header's record length is invalid");
  }
  if (!Array.isArray(blobs)) {
    throw new Malformed("the upload header's blobs are not a list");
  }
  const parsed: UploadBlob[] = [];
  for (const blob of blobs) {
    if (!Array.isArray(blob) || blob.length !== 3) {
      throw new Malformed("an upload blob is not [hash, raw length, compressed length]");
    }
    const [hash, raw, compressed] = blob as unknown[];
    if (!isHash(hash)) {
      throw new Malformed("an upload blob's hash is not 64 lowercase hex digits");
    }
    if (!isLength(raw)) {
      throw new Malformed(`blob ${hash}'s raw length is invalid`);
    }
    if (compressed !== null && (!isLength(compressed) || compressed < ZSTD_MAGIC.length)) {
      throw new Malformed(`blob ${hash}'s compressed length is invalid`);
    }
    const previous = parsed[parsed.length - 1];
    if (previous && previous.hash >= hash) {
      throw new Malformed("the upload's blobs are not sorted by hash and unique");
    }
    parsed.push({ hash, raw, compressed });
  }
  return { record, blobs: parsed };
}

/** Reads a whole upload body. With `key`, its record must be the result
 * record for that key. Throws `Malformed` when the body is not an upload. */
export function parseUpload(body: Uint8Array, key?: string): Upload {
  if (body.length < 4) {
    throw new Malformed("the upload body ends early");
  }
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  const headerLength = view.getUint32(0, true);
  if (headerLength > LARGEST_HEADER || 4 + headerLength > body.length) {
    throw new Malformed("the upload header's length is invalid");
  }
  const header = parseHeader(body.subarray(4, 4 + headerLength));
  let at = 4 + headerLength;
  const inlineTotal = header.blobs.reduce((total, blob) => total + (blob.compressed ?? 0), 0);
  if (at + header.record + inlineTotal !== body.length) {
    throw new Malformed("the upload body's length does not match its header");
  }
  const record = body.subarray(at, at + header.record);
  at += header.record;
  let json: unknown;
  try {
    json = JSON.parse(utf8.decode(record));
  } catch {
    throw new Malformed("the record is not JSON");
  }
  if (typeof json !== "object" || json === null || Array.isArray(json)) {
    throw new Malformed("the record is not a JSON object");
  }
  if (key !== undefined && (json as { key?: unknown }).key !== key) {
    throw new Malformed(`the record is not for key ${key}`);
  }
  const inline = new Map<string, { offset: number; length: number }>();
  for (const blob of header.blobs) {
    if (blob.compressed === null) {
      continue;
    }
    if (!ZSTD_MAGIC.every((byte, index) => body[at + index] === byte)) {
      throw new Malformed(`blob ${blob.hash} is not a zstd frame`);
    }
    inline.set(blob.hash, { offset: at, length: blob.compressed });
    at += blob.compressed;
  }
  return { header, record, json: json as Record<string, unknown>, inline };
}

/** Builds an upload body: the counterpart of `parseUpload`, for tests and
 * tools. `frames` holds the zstd frame of each blob carried inline. */
export function buildUpload(
  record: Uint8Array,
  blobs: { hash: string; raw: number; frame?: Uint8Array }[],
): Uint8Array {
  const header = new TextEncoder().encode(
    JSON.stringify({
      record: record.length,
      blobs: blobs.map((blob) => [blob.hash, blob.raw, blob.frame ? blob.frame.length : null]),
    }),
  );
  const parts = [u32(header.length), header, record, ...blobs.flatMap((blob) => (blob.frame ? [blob.frame] : []))];
  return concat(parts);
}

/** Where a pack carries a blob. */
export const INLINE = 1;
export const APART = 0;

/** The decoded bytes of a v4 pack up to its inline blobs: `"qkp4"`, the
 * record's length and the record, the blob count, and each blob's placement
 * and raw length. */
export function packHeader(record: Uint8Array, tags: { placement: number; raw: number }[]): Uint8Array {
  const bytes = new Uint8Array(4 + 8 + record.length + 4 + tags.length * 9);
  const view = new DataView(bytes.buffer);
  bytes.set(PACK_MAGIC, 0);
  view.setBigUint64(4, BigInt(record.length), true);
  bytes.set(record, 12);
  let at = 12 + record.length;
  view.setUint32(at, tags.length, true);
  at += 4;
  for (const tag of tags) {
    bytes[at] = tag.placement;
    view.setBigUint64(at + 1, BigInt(tag.raw), true);
    at += 9;
  }
  return bytes;
}

export function u32(value: number): Uint8Array {
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setUint32(0, value, true);
  return bytes;
}

export function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((total, part) => total + part.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/** Reads a byte stream in parts of known length. */
export class ByteReader {
  private reader: ReadableStreamDefaultReader<Uint8Array>;
  private buffered: Uint8Array = new Uint8Array(0);

  constructor(stream: ReadableStream<Uint8Array>) {
    this.reader = stream.getReader();
  }

  /** The next `length` bytes. Throws when the stream ends first. */
  async read(length: number): Promise<Uint8Array> {
    const parts: Uint8Array[] = [];
    let wanted = length;
    while (wanted > 0) {
      const chunk = await this.take(wanted);
      parts.push(chunk);
      wanted -= chunk.length;
    }
    return parts.length === 1 ? parts[0]! : concat(parts);
  }

  /** The next `length` bytes, in the chunks they arrive in. */
  async *chunks(length: number): AsyncGenerator<Uint8Array> {
    let wanted = length;
    while (wanted > 0) {
      const chunk = await this.take(wanted);
      wanted -= chunk.length;
      yield chunk;
    }
  }

  async skip(length: number): Promise<void> {
    for await (const _ of this.chunks(length)) {
      // Dropped.
    }
  }

  async cancel(): Promise<void> {
    await this.reader.cancel();
  }

  /** Up to `most` bytes, at least one. */
  private async take(most: number): Promise<Uint8Array> {
    while (this.buffered.length === 0) {
      const { done, value } = await this.reader.read();
      if (done) {
        throw new Error("stored object ends early");
      }
      this.buffered = value;
    }
    const chunk = this.buffered.subarray(0, most);
    this.buffered = this.buffered.subarray(chunk.length);
    return chunk;
  }
}
