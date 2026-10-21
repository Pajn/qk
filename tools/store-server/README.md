# qk store server

The reference store server for qk's remote cache: a Cloudflare Worker over an
R2 bucket, with one Durable Object holding the index. It speaks the protocol
in [PROTOCOL.md](PROTOCOL.md). Compared with plain S3 storage, it leaves out of
each fetched entry the blobs the client already holds, and keeps the index
itself instead of leaving clients to list and merge it.

The Worker never compresses or decompresses. It stores upload bodies as
clients send them and composes each pack from a raw zstd frame of the pack
header followed by the client's own zstd frames.

## Layout

| Path | What |
| --- | --- |
| `src/worker.ts` | Routing, authentication and every endpoint |
| `src/frames.ts` | Raw zstd frames, upload bodies, pack headers |
| `src/bloom.ts` | Session Bloom filters |
| `src/index-do.ts` | The `Index` Durable Object (SQLite) |
| `src/auth.ts` | Bearer tokens checked against SHA-256 digests |

R2 holds `entries/<key>`, `warm/<a>/<b>` and `pathsets/<weak>` (upload bodies), `blobs/<hash>`
(one zstd frame each) and `sessions/<id>` (Bloom filters).

## Deploying

This needs a Cloudflare account with R2 and Durable Objects, and Wrangler
logged in to it.

```sh
pnpm install
pnpm wrangler r2 bucket create qk-store
# Sessions last a day; let R2 delete their filters after that.
pnpm wrangler r2 bucket lifecycle add qk-store sessions sessions/ --expire-days 2
```

Tokens are kept as SHA-256 digests, comma-separated, lowercase hex. A read
token may fetch; a write token may also store. Compute a digest with:

```sh
printf %s "$TOKEN" | shasum -a 256
```

Then set both secrets and deploy:

```sh
pnpm wrangler secret put READ_TOKENS
pnpm wrangler secret put WRITE_TOKENS
pnpm run deploy
```

## Configuring qk

Point qk at the Worker in `nx.json`:

```json
{
  "storeServer": { "url": "https://qk-store-server.<subdomain>.workers.dev" }
}
```

and give it a token in `QK_STORE_SERVER_TOKEN`: a write token where results
should be stored, such as CI on the main branch, a read token elsewhere. Where
a read token is used, set that mode to `read` too, so qk does not try to
upload:

```json
{
  "storeServer": { "url": "…", "localMode": "read", "ciMode": "read-write" }
}
```

## Developing

```sh
pnpm dev        # local server
pnpm test       # tests, in the Workers runtime
pnpm typecheck
```

## Limits

- Upload bodies (`PUT entries/…`, `PUT warm/…`) are read into memory, up to
  100 MiB, and path sets (`PUT pathsets/…`) up to 16 MiB; Cloudflare's own request size limit for the account's plan applies
  first. Blobs stored apart (`PUT blobs/…`) stream to R2.
- A pack carries inline at most 64 MiB of blobs stored apart; the rest the
  client fetches by hash.
