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

Entries are stored under `<cacheKeyPrefix>qk/v4/`, so a bucket shared with Nx
never mixes the two. Result entries and portable warm state use streaming
Zstandard level 3 compression with a frame checksum. Each entry is one object
holding its manifest and its small outputs, so that a lookup or restore of an
entry whose outputs are all small is one request however many files the task
wrote. A local miss fetches it, keeps the
outputs the local cache lacks, verifying each against its hash, and shows
`[remote cache]`; any failure is a miss. The path sets of tasks with
[`qk:ultracache`](ultracache.md) are stored as `pathsets/<weak>`. A store
server takes at most 16 MiB of them for one task, so a larger record is
shared without its oldest sets; one whose newest set alone is larger is not
shared, and its upload fails.

What tasks are expected to take, which orders them and sizes their
[shares of the cores](threads.md), is stored as `expectations/<name>`, one
record per branch and platform. It holds each task's last five runs that
executed it: their time, the cores they held and the memory they used. A run
that executed tasks adds them to the record of its branch, starting from the
default branch's when its branch has none, and saves it for its branch; the
newest save replaces the last, and a task no run has executed for 30 days is
dropped. A run whose local history lacks a task reads the record of its
branch, else of the default branch, so that a machine without history, such
as a CI job on a fresh clone, still knows which tasks take long. Local history
wins for a task it has. A record that cannot be read leaves the run without
it, and is not replaced. The record is fetched while the run gets ready to
start its tasks, and waited for only when local history lacks one of them.

Expectations are read and written as the store's mode allows.
`QK_REMOTE_EXPECTATIONS` narrows that: `read` (also spelled `read-only`) reads
them without adding to them, `off` neither reads nor writes them, and
`read-write`, the default, changes nothing.

An output whose compressed size reaches 1 MiB is stored apart, as
`blobs/<hash>`, uploaded once for every entry that has it and only when the
store lacks it. A machine fetches it only when its local cache lacks it, so a
machine that already holds a build's large files receives little more than the
manifest. The index names each entry's large outputs, so a machine lacking
them fetches them beside the entry, a few at a time, rather than after it.
`QK_REMOTE_BLOB_THRESHOLD` sets another size, such as `256KB`, or `off` to keep
every output in its entry. Large outputs are shared between entries, so do not
expire `blobs/` with a storage lifecycle rule: an entry whose output is gone is
a miss, never a wrong result, but every entry citing it misses until it runs
again. After a task is saved it is uploaded in the background, and a run waits
for its uploads before it ends. Uploads report their failures without failing
the run; an entry larger than 5 GiB, the most one S3 upload may hold, is not
uploaded.

A request may take ten minutes, which allows for the largest transfers, but
one that receives nothing from the store for 30 seconds fails: a store that
stops answering costs a lookup a miss and an upload its failure, rather than
holding every task that asks it. Once the store has stalled twice, the run
asks it nothing more: each later lookup misses without asking, each later
upload fails, and qk says so once. Until then, when the connection fails an
upload, by a reset, a broken pipe or a stall, qk sends it twice more, after
two and then four seconds, and stops trying once the store is left out. An
upload the store refuses, or one that runs out its ten minutes, fails at
once.

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

The remote format uses a clean cutover from `qk/v3`: new clients neither fetch
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
at 8 MiB each and 32 MiB across a synchronization; exceeding either limit also
uses direct lookups without retaining an incomplete index. An entry uploaded
after a run started is not found by that run. Once there are
16 or more listings, a writing run merges them into its own and deletes
them, so the store's credentials need permission to delete objects. Entries
whose timestamps have not been renewed for 30 days drop out of the index.
Read-write local cache hits renew entries an available remote index already
lists; unuploaded local results are not advertised.

A listing also records its run: the entries it used, in the order it used
them. Merging keeps the most recent runs, at most 32, none older than 14 days.
A cached chain of tasks otherwise fetches one entry per level, each waiting for
the level below. Once a task's entry is a remote hit, qk fetches the other
entries of the recent runs that used it in the background, nearest to it
first, so a dependent usually finds its entry already local; a task whose
entry is still being fetched waits for that fetch. Prefetching stops when
fewer than half of the run's entries match the runs it follows, after a
gigabyte or 5,000 entries, or when the run ends, without waiting for it. A
prefetched entry is shown as `[remote cache]`, and listed, only once a task
uses it. `QK_REMOTE_PREFETCH=off` turns it off.

## Store server

nx.json's `qk:storeServer` key sends the remote cache to a store server
instead of S3, with its token in `QK_STORE_SERVER_TOKEN`:

```jsonc
{
  "qk:storeServer": { "url": "https://cache.example.com" }
}
```

A store server keeps the same entries, but each run first tells it which
outputs the local cache holds, and it leaves those out of every entry it sends.
A run whose request to tell it fails says why, and receives entries whole,
as does, silently, one whose local cache holds no outputs or more than two
million. It keeps the index itself and commits each entry once, so runs
neither write announcements nor merge listings. `localMode`, `ciMode`, `uploadMode` and
`NX_POWERPACK_CACHE_MODE` apply as they do for `s3`. A read token fetches; a
write token also uploads.

Nx does not read `qk:storeServer`, so a workspace that still runs some tasks
with Nx can keep `s3` for Nx's cache plugin: qk uses the store server and
ignores `s3`.
`tools/store-server` in qk's repository is a reference server, a Cloudflare
Worker over R2, and documents the protocol in its `PROTOCOL.md`.

### Store proxy

Experimental. `qk store proxy` serves a store server's protocol to machines
on one network, in front of the real one, so that what they fetch often
travels over the local network rather than from the store server:

```sh
qk store proxy --upstream https://cache.example.com --dir /var/cache/qk-proxy --max-size 200GB
```

Machines point `QK_STORE_SERVER_URL` at it, `http://<host>:7077` by default,
and keep their own tokens. Outputs never change once stored, so the proxy
keeps those that pass through it on its disk: those it fetches for a
machine, those a machine uploads, and those an uploaded entry or warm state
carries inline. It checks each against its hash before keeping it, and
serves it again to any machine whose token the store server has accepted.
Everything else, entries, warm state records, writes and the index, goes to
the store server as it came, so what a machine finds is what the store
server holds. Past `--max-size` the proxy removes the outputs used least
recently. `GET /proxy/stats` reports how much it served itself and how much
it fetched. While the proxy does not answer, runs on those machines get no
remote cache.

## Storage limits

The cache stays under a size limit: `QK_MAX_CACHE_SIZE`, else
`NX_MAX_CACHE_SIZE`, else nx.json
`maxCacheSize`, else a tenth of the disk holding it (100GB when the disk's
size cannot be read), as in Nx's code; its documentation's 10GB cap is not
in the code. Sizes are a number of bytes with an optional `KB`, `MB` or `GB`,
in powers of 1024; `0` means unlimited. After each run, and on
`qk cache prune [--max-size <size>]`, qk evicts the entries unused for a
week, whatever the limit, and when the cache is over the limit the least
recently used entries until it holds at most 90% of it, as Nx does. A hit
counts as a use. Outputs shared between entries are stored once and deleted only
with the last entry citing them. Stored outputs no entry cites, scratch files
and locks of evicted entries are removed once they are an hour old, since a
concurrent run may still be writing them; outputs a remote fetch in progress
relies on are kept whatever their age. A run under the limit only sums the
cache's size; the full pass runs at most hourly unless the cache is over its
limit.

`NX_CACHE_DIRECTORY`, else nx.json `cacheDirectory`, moves the cache, as in
Nx: a path relative to the workspace root, with qk's entries in `qk/v1`
inside it so they never mix with Nx's, and so CI steps that save and restore
that directory keep working. A relative path gives each worktree its own
cache; the run history and each worktree's state stay in the git directory.

`QK_CACHE_DIRECTORY` and `QK_MAX_CACHE_SIZE` set the same for qk alone, ahead
of the Nx settings, so a machine running both can give each its own place and
limit. `QK_CACHE_DIRECTORY` is qk's own, so its entries go in `v1` directly
inside it.
