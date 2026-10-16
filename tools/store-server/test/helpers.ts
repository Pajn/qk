import { SELF } from "cloudflare:test";
import { buildUpload, concat, INLINE, packHeader, rawFrame } from "../src/frames";

export const READ = "read-token";
export const WRITE = "write-token";

export function request(
  method: string,
  path: string,
  options: { token?: string | null; body?: BodyInit; headers?: Record<string, string> } = {},
): Promise<Response> {
  const headers: Record<string, string> = { ...options.headers };
  const token = options.token === undefined ? WRITE : options.token;
  if (token !== null) {
    headers.Authorization = `Bearer ${token}`;
  }
  return SELF.fetch(`https://store.test/v4/${path}`, { method, headers, body: options.body });
}

/** A fresh hash, so tests never share an object. */
export function hash(): string {
  return [...crypto.getRandomValues(new Uint8Array(32))].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export function bytes(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

/** A blob in a test upload: its hash, raw bytes, and whether it is carried
 * inline (otherwise it is stored apart). */
export interface TestBlob {
  hash: string;
  raw: Uint8Array;
  inline: boolean;
}

export function blob(text: string, inline: boolean): TestBlob {
  return { hash: hash(), raw: bytes(text), inline };
}

export function sorted(blobs: TestBlob[]): TestBlob[] {
  return [...blobs].sort((a, b) => (a.hash < b.hash ? -1 : 1));
}

/** A result record for `key` citing `blobs`. */
export function record(key: string, blobs: TestBlob[]): Uint8Array {
  const artifacts = Object.fromEntries(blobs.map((blob, index) => [`out/${index}`, { blob: blob.hash }]));
  return bytes(JSON.stringify({ version: 1, key, artifacts }));
}

/** The upload body of `record` with `blobs`, each frame a raw zstd frame. */
export function upload(record: Uint8Array, blobs: TestBlob[]): Uint8Array {
  return buildUpload(
    record,
    sorted(blobs).map((blob) => ({ hash: blob.hash, raw: blob.raw.length, frame: blob.inline ? rawFrame(blob.raw) : undefined })),
  );
}

/** What a pack of `record` decodes to, carrying inline the blobs `inline`
 * names. */
export function expectedPack(record: Uint8Array, blobs: TestBlob[], inline: (blob: TestBlob) => boolean): Uint8Array {
  const ordered = sorted(blobs);
  const tags = ordered.map((blob) => ({ placement: inline(blob) ? INLINE : 0, raw: blob.raw.length }));
  return concat([packHeader(record, tags), ...ordered.filter(inline).map((blob) => blob.raw)]);
}

export async function putApart(blob: TestBlob): Promise<void> {
  const response = await request("PUT", `blobs/${blob.hash}`, { body: rawFrame(blob.raw) });
  if (response.status !== 204) {
    throw new Error(`PUT blobs/${blob.hash}: ${response.status} ${await response.text()}`);
  }
}

export function hex(data: Uint8Array): string {
  return [...data].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
