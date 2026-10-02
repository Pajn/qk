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

Entries are stored under `<cacheKeyPrefix>qk/v2/`, so a bucket shared with Nx
never mixes the two. Each entry is one object holding its manifest and
outputs, so that a lookup or restore is one request however many files the
task wrote. A local miss fetches it, keeps the outputs the local cache lacks,
verifying each against its hash, and shows `[remote cache]`; any failure is a
miss. After a task is saved it is uploaded in the background, and a run waits
for its uploads before it ends. Uploads report their failures without failing
the run; an entry larger than 5 GiB, the most one S3 upload may hold, is not
uploaded.

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
