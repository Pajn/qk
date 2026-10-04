# Remote cache and storage limits

nx.json's `s3` key, as `@nx/s3-cache` reads it, adds a remote store on
S3-compatible storage: `bucket`, `region`, `endpoint`, `forcePathStyle`,
`cacheKeyPrefix`, `accessKeyId` and `secretAccessKey`. Credentials otherwise
come from `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and
`AWS_SESSION_TOKEN`, including from the workspace's `.env` files. `localMode`
applies outside CI and `ciMode` when `CI` is set; each is `read-write` (the
default), `read` (also spelled `read-only`) or `no-cache`, and
`NX_POWERPACK_CACHE_MODE` overrides both. `encryptionKey` and SSO profiles are
not supported; a store that cannot be used is reported and left out, and the
local cache carries on.

Entries are stored under `<cacheKeyPrefix>qk/v3/`, so a bucket shared with Nx
never mixes the two. Result entries and portable warm state use streaming
Zstandard level 3 compression with a frame checksum. Each entry is one object
holding its manifest and outputs, so that a lookup or restore is one request however many files the
task wrote. A local miss fetches it, keeps the outputs the local cache lacks,
verifying each against its hash, and shows `[remote cache]`; any failure is a
miss. After a task is saved it is uploaded in the background, and a run waits
for its uploads before it ends. Uploads report their failures without failing
the run; an entry larger than 5 GiB, the most one S3 upload may hold, is not
uploaded.

For local runs, qk can hand queued uploads to a detached process after local
cache saves finish. Set `s3.uploadMode` to `"background"` to opt in:

```jsonc
{
  "s3": {
    "bucket": "task-cache",
    "region": "us-east-1",
    "uploadMode": "background"
  }
}
```

`"wait"` is the default. `QK_REMOTE_UPLOAD_MODE=wait` overrides the setting
for a run; use it in CI so uploads finish before the runner is torn down.
It waits for that run's uploads, not workers started by earlier runs.
Background mode starts uploads at the end of the run, whereas the default
mode overlaps uploads with task execution and waits for them at the end.

The detached process receives a private snapshot of result manifests and
warm records, with hard links to immutable cache blobs (copies when hard
links are unavailable). It can finish after qk exits, even if the local
cache is pruned or reset. Snapshots and private failure logs live beside the
cache directory; qk prints the log path on handoff. Credentials go through
a pipe, not command arguments or a request file. If handoff fails, qk waits
for the uploads instead.

The worker removes its snapshot when it finishes and writes a completion
line with a failure count to the log. This is best-effort background work,
not a durable retry queue: shutdown, worker termination, or CI job cleanup
can interrupt it. Later background runs reclaim snapshots older than one
day when no worker holds their lease, and logs older than one day. Pending
snapshots can retain disk space outside the cache size limit until cleanup.

The remote format uses a clean cutover from `qk/v2`: new clients neither fetch
nor overwrite older remote objects. Upgrading may cause one-time remote
misses while the compressed cache fills. Existing local entries are unchanged,
and older clients can continue using their own remote namespace. There is no
legacy lookup after a miss. Downloads limit the decoder window to 128 MiB and
the expanded pack to 5 GiB; malformed, truncated or oversized packs are misses.
The local content-addressed blob store remains uncompressed.

Every request to the store waits a round trip, so qk keeps an index of the
entries it holds and does not ask for one the index leaves out. Each run
that writes to the store adds a listing under `index/` of the entries it
found or put there. Each entry upload also writes a listing before
publishing the entry, so an interrupted run or a failed final listing cannot
leave a stored entry undiscoverable. After the final listing succeeds, the
run removes its replaced announcements in batches. If that first listing
fails, the entry is not uploaded; if the entry upload fails, its listed key
simply returns a miss. A run lists `index/` as it starts, reads the listings it
has not seen and keeps them in the local cache. If that fails, including a
listing removed by concurrent compaction, it looks up each entry as it needs
it. Paginated indexes also use direct lookups because pages cannot establish
a complete snapshot during concurrent compaction. Listing payloads are bounded
at 4 MiB each and 32 MiB across a synchronization; exceeding either limit also
uses direct lookups without retaining an incomplete index. An entry uploaded
after a run started is not found by that run. Once there are
16 or more listings, a writing run merges them into its own and deletes
them, so the store's credentials need permission to delete objects. Entries
whose timestamps have not been renewed for 30 days drop out of the index.
Read-write local cache hits renew entries an available remote index already
lists; unuploaded local results are not advertised.

The cache stays under a size limit: `NX_MAX_CACHE_SIZE`, else nx.json
`maxCacheSize`, else a tenth of the disk holding it, as in Nx. Sizes are a
number of bytes with an optional `KB`, `MB` or `GB`, in powers of 1024; `0`
means unlimited. After each run, and on `qk cache prune [--max-size <size>]`,
qk evicts the least recently used entries until the cache fits. A hit counts
as a use. Outputs shared between entries are stored once and deleted only
with the last entry citing them. Stored outputs no entry cites, scratch files
and locks of evicted entries are removed once they are an hour old, since a
concurrent run may still be writing them. A run under the limit only sums the
cache's size; the full pass runs at most hourly unless the cache is over its
limit.

`NX_CACHE_DIRECTORY`, else nx.json `cacheDirectory`, moves the cache, as in
Nx: a path relative to the workspace root, with qk's entries in `qk/v1`
inside it so they never mix with Nx's, and so CI steps that save and restore
that directory keep working. A relative path gives each worktree its own
cache; the run history and each worktree's state stay in the git directory.
