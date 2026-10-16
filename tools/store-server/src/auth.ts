// Bearer tokens. The server keeps only SHA-256 digests of them, as
// comma-separated lowercase hex in the READ_TOKENS and WRITE_TOKENS secrets.

/** What a request's token allows. */
export type Access = "read" | "write";

/** The access `request`'s bearer token grants, or `null` when it has none or
 * the server does not know it. A write token can also read. */
export async function access(
  request: Request,
  env: { READ_TOKENS?: string; WRITE_TOKENS?: string },
): Promise<Access | null> {
  const header = request.headers.get("Authorization");
  const match = header?.match(/^Bearer\s+(\S+)\s*$/i);
  if (!match?.[1]) {
    return null;
  }
  const digest = await sha256Hex(match[1]);
  if (digests(env.WRITE_TOKENS).has(digest)) {
    return "write";
  }
  if (digests(env.READ_TOKENS).has(digest)) {
    return "read";
  }
  return null;
}

/** The lowercase hex SHA-256 digest of `text` as UTF-8. */
export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

function digests(list: string | undefined): Set<string> {
  return new Set(
    (list ?? "")
      .split(",")
      .map((digest) => digest.trim().toLowerCase())
      .filter((digest) => /^[0-9a-f]{64}$/.test(digest)),
  );
}
