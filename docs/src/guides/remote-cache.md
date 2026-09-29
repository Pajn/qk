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

Entries are stored under `<cacheKeyPrefix>qk/v1/` in the local layout, so a
bucket shared with Nx never mixes the two. A local miss fetches the manifest
and only the outputs the local cache lacks, verifying each against its hash,
and shows `[remote cache]`; any failure is a miss. After a task is saved it is
uploaded in the background, outputs first and the manifest last, and a run
waits for its uploads before it ends. Uploads report their failures without
failing the run.

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
