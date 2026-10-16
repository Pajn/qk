# qk store server protocol, v4

A store server answers qk's remote store requests itself, in front of the
storage that holds result records, warm-state records and blobs. It speaks the
same pack format as plain S3 storage (`crates/qk-cache/src/remote/pack.rs`), so
a fetched entry is read the same way, but it can leave out the blobs a client
already holds and keep the index itself.

## Requests

Every path is under `<url>/v4/`. Every request carries
`Authorization: Bearer <token>`. A read token may make every `GET` and `HEAD`
request, `POST sessions` and `POST blobs/missing`; a write token may make every
request.

Errors are JSON, `{"error": "<code>", "message": "<text>"}`:

| Status | Code | Meaning |
| --- | --- | --- |
| 400 | `malformed` | The request cannot be read |
| 401 | `unauthorized` | The token is missing or unknown |
| 403 | `read_only` | A read token asked to write |
| 404 | `not_found` | The object does not exist |
| 409 | `exists` | The entry was committed already; the upload succeeded |
| 413 | `too_large` | The body is larger than the server accepts |
| 422 | `missing_blobs` | A commit cites blobs stored apart that the server lacks; the body adds `"blobs": [<hash>, …]` |
| 429, 503 | `busy` | Try later, after `Retry-After` seconds |

Keys and blob hashes are 64 lowercase hex digits. `<a>/<b>` in a warm path is
two 32-hex-digit names, as plain S3 storage names warm records.

## Framing

A **zstd frame** is one Zstandard frame. A **raw frame** is a Zstandard frame
whose blocks are all raw (uncompressed), which a server can write without a
codec: the magic `28 B5 2F FD`, the frame header descriptor `00`, the window
descriptor `38` (128 KiB), then blocks of at most 128 KiB, each a 3-byte little
endian header `size << 3 | type << 1 | last` with type 0, followed by its bytes.
A raw frame of nothing is one empty last block, `01 00 00`.

An **upload body** (`application/x-qk-upload`) is how a client sends a result
or warm record with the blobs it carries:

```text
u32 LE length of the header, then the header, JSON:
    {"record": <length of the record>,
     "blobs": [[<hash>, <raw length>, <compressed length or null>], …]}
the record, raw JSON
for each blob whose compressed length is not null, in order: a zstd frame of it
```

`blobs` lists every blob the record cites, in the order the record's blob set
sorts (lexically by hash). A server accepts upload bodies of up to 100 MiB, so a
client carries at most 64 MiB of blobs inline, counted before compression, and
stores the rest apart. A blob with a compressed length is carried inline;
one without is stored apart, as `blobs/<hash>`, and must be there before the
record is committed.

A **pack** (`application/x-qk-pack`) is any sequence of frames that decodes to
the v4 pack in `pack.rs`: `"qkp4"`, the record's length and the record, the
blob count, a placement and raw length per blob, then the inline blobs' bytes.
A server writes the part up to the inline bodies as one raw frame, then copies
each inline blob's stored zstd frame unchanged.

## Endpoints

### `POST sessions`

Body: a Bloom filter of the blobs the client holds (`application/x-qk-bloom`).
Answer: `201 {"session": "<32 hex digits>", "expires": <milliseconds>}`. A
session lives at least an hour.

The filter is `u32 LE version (1)`, `u32 LE k`, `u64 LE m` (bits), then
`ceil(m / 8)` bytes, bit `i` being `byte[i >> 3] & (1 << (i & 7))`. For a blob
whose hash is the 32 bytes `H`, with `h1` the little-endian `u64` of `H[0..8]`
and `h2` that of `H[8..16]` with its lowest bit set, the bits are
`(h1 + i × h2) mod m` for `i` from 0 to `k − 1`, in wrapping 64-bit arithmetic.
Clients use `k = 7` and size `m` for a 1% false-positive rate.

### `GET entries/<key>[?session=<id>]`, `GET warm/<a>/<b>[?session=<id>]`

Answer: `200`, a pack; or `404`.

Each blob is inline (placement 1) unless the session's filter says the client
holds it, in which case it is apart (placement 0). A blob the server stores
apart is inline only while the response's inline blobs stored apart total at
most 64 MiB compressed; past that it is apart. Without a session, or with an
unknown or expired one, nothing is taken as held.

A client that lacks a blob the pack marks apart fetches it by hash. The filter
can be wrong, so a client checks what it holds and asks for the rest.

### `GET blobs/<hash>[?entry=<key>|?warm=<a>/<b>]`, `HEAD blobs/<hash>`

Answer: `200`, the blob as one zstd frame; or `404`. With `entry` or `warm`, a
blob that record carries inline is found there too.

### `POST blobs/missing`

Body: `{"blobs": [<hash>, …]}`, at most 4,096. Answer:
`{"missing": [<hash>, …]}`, the ones the server does not store apart.

### `PUT blobs/<hash>`

Write token. Body: the blob as one zstd frame. Answer: `204`. The server cannot
check the hash; clients check every blob they keep.

### `HEAD entries/<key>`

Answer: `200` or `404`.

### `PUT entries/<key>`

Write token. Body: an upload body whose record's `key` is `<key>`. Answer:
`204`; `409` when an entry was committed first, which is success; `422` with
the blobs stored apart that the server lacks, after which the client uploads
them and commits again.

### `PUT warm/<a>/<b>`

Write token. Body: an upload body. The newest save replaces the last. Answer:
`204`, or `422` as for entries.

### `GET index[?since=<cursor>]`

Answer: `200`, text, with the header `Qk-Index-Cursor: <cursor>`. Lines are
`<key> <milliseconds>` for every key whose time changed after `cursor` (all
keys without one), then `run <id> <milliseconds> <member> …` for every run the
server remembers, in the format of `crates/qk-cache/src/remote/index.rs`. A
cursor is opaque to clients.

### `POST runs`

Write token. Body: text, `<key> <milliseconds>` for each key the run found in
the store or put there, then one run line. The server renews those keys'
times, remembers the run, and keeps at most 32 runs, none older than 14 days,
naming at most 100,000 keys together. Answer: `204`.
