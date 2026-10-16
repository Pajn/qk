---
status: accepted
---

# Content-addressed remote store, run-grouped prefetch, and a store server

The remote store keeps each result record as one compressed object holding the
record and every blob it cites. A hit is therefore one request, but it costs in
two ways. A machine with a good local cache downloads blobs it already holds:
the entry is missing locally, its files mostly are not. And a cached chain of
tasks fetches one entry per level, each a round trip that cannot start until
the level below is restored, because a dependent's cache key is not known
before then. In a large monorepo's CI, a fully cached build spent most of a
3.3s critical path in about ten such steps of 100–200ms each.

We will:

1. **Store large blobs by content hash.** A result record keeps small blobs
   inline, so a hit on a small entry stays one request. A large blob is its own
   object, fetched only when the local cache lacks it and uploaded only when
   the remote store lacks it. This is a new layout, `qk/v4/`.
2. **Keep run listings as runs, and prefetch from them.** Each run's listing
   names the keys that run used together, but today a writer compacts the
   index by merging every listing into one key set and deleting the
   originals, so the grouping is lost in the store itself. Compaction will keep
   the flat key set for lookups and, beside it, a bounded history of recent
   runs, each with its identity, time and member keys. Once a run's first key
   is a remote hit, qk fetches the other entries of recent runs that used that
   key in the background, so a dependent usually finds its entry already local.
3. **Offer a store server beside plain S3.** A small HTTP service in front of
   object storage, with a reference implementation as a Cloudflare Worker over
   R2. It can do what an object store cannot:
   - take a summary of the client's local blobs once per run, such as a Bloom
     filter, and answer each fetch with the record and the blobs the summary
     says the client lacks, usually in one request;
   - prefetch on the server, returning with an entry the records that recent
     runs fetched after it;
   - keep an authoritative index, run history included, and commit an entry
     atomically, which replaces the listings, announcements and compaction the
     client needs on plain S3;
   - serve hot objects from an edge cache.

   Plain S3 stays supported, and gets 1 and 2.

## A hit is complete before it is kept

Whatever path a fetch takes, the client treats a result record as fetched only
once every blob it cites is in the local cache and verified against its hash,
and it publishes the record locally only then. This matters for the store
server, whose summary of local blobs is a hint, not a promise: a Bloom filter
has false positives, so the server can leave out a blob the client lacks, and
the local cache can prune a blob after the summary was sent. The client checks
the record against its local cache, requests any missing blobs by hash in one
follow-up wave of requests, and keeps blobs an in-flight fetch relies on safe from
pruning until the record is published. A hit on a store server therefore
costs one request when the summary is right and one more wave when it is not, and never becomes an execution
miss because of it.

## What makes a blob large

A blob is stored on its own when its compressed size, what it would cost on the
wire, reaches a threshold. Raw size would mislead: generated code and source
maps compress many times over, so a large file can be a small transfer that is
not worth its own request. The threshold is where transferring the blob costs
more than a round trip, at the bandwidth and latency CI runners see, and it is
set from measurement rather than fixed here.

An entry's missing external blobs are fetched with bounded concurrency. An
entry with more missing blobs than the limit needs more than one wave of
requests on plain S3; the benchmark gate below decides whether that cost is
acceptable.

Before `qk/v4/` becomes the default, a cold runner restoring a cached chain
must be no slower than with today's one-object packs, measured in the
benchmark's restoration matrix and on a real CI run. If it is slower, the
threshold moves up until it is not; the layout must not regress the CI case
that motivated it.

## Considered options

- **Key dependents before their dependencies' blobs arrive.** A dependent's key
  needs only its dependency's output fingerprint, which the result record
  holds. But input hashing reads dependency outputs from disk, so this needs
  hashing to read pending files from records, a wait before any task executes,
  restore timestamps that keep dependents newer than their dependencies, and a
  rule for a download that fails after dependents were keyed on it. Each is a
  way to produce a stale build. Prefetching attacks the same round trips, and a
  wrong guess costs only a download.
- **Byte-range reads of a pack.** Compressing each blob on its own and reading
  only the missing ranges avoids a second request when nothing is held, but S3
  serves one range per request, so a pack with several missing blobs costs
  several requests. Inline small blobs and separate large ones get most of the
  saving with a simpler format.
- **Skip held blobs within today's one request.** Not possible: the client
  learns which blobs an entry cites from its record, which arrives in the same
  response as the blobs.
- **Keep run history in separate objects.** Writing each run's membership to
  its own object would avoid changing compaction, but readers would list and
  fetch a growing set of small objects, the cost compaction exists to avoid.

## Consequences

- Moving to `qk/v4/` leaves every machine with a cold remote cache once. Older
  qk versions keep reading and writing `qk/v3/` without conflict, but share no
  results with machines on the new layout until they upgrade.
- A fresh CI runner pays extra requests on a hit whose large blobs it lacks,
  fetched with bounded concurrency, so possibly more than one wave. The
  threshold and the benchmark gate above bound that cost.
- Prefetching downloads entries no task may use; it needs a size limit and
  should stop once a run's keys diverge from the runs it is following.
- The run history in the index is bounded, so prefetch follows recent runs
  only; a branch that has not run in a while prefetches nothing.
- The store server is a second implementation of the remote protocol to keep
  correct. Plain S3 remains the reference for what a result means; the server
  only changes how bytes travel.
