// The index: which entries the store holds, with when a run last found each,
// and the runs that used them. One Durable Object holds it all in SQLite, so
// every update is ordered and a cursor names a point in that order. The line
// formats are those of crates/qk-cache/src/remote/index.rs.
//
// It also holds which blobs are stored apart, with their sizes, so that a
// record citing thousands of them is checked and composed with a few queries
// rather than a bucket request per blob. Blobs stored before it held them are
// listed from the bucket once, in the background.

import { DurableObject } from "cloudflare:workers";

const DAY = 24 * 60 * 60 * 1000;
/** How long a key stays listed after the last run that found its entry. */
const KEPT_FOR = 30 * DAY;
/** How many runs the index remembers, how long, and how many keys they may
 * name together; and how many keys one run may name. */
export const HISTORY_RUNS = 32;
const HISTORY_FOR = 14 * DAY;
const HISTORY_MEMBERS = 100_000;
const RUN_MEMBERS = 20_000;
const MEMBER = /^[0-9a-fA-F]{16}$/;
const KEY = /^[0-9a-f]{64}$/;
const TIME = /^\d{1,16}$/;
/** SQLite in a Durable Object binds at most 100 parameters to a query. */
const BOUND = 100;
/** Bucket listings, of up to 1,000 objects each, one alarm takes. */
const LISTINGS_PER_ALARM = 20;
/** How long after a listing failed it is tried again. */
const RELIST_AFTER = 60 * 1000;

/** Where a blob stored apart is: on its own as `blobs/<hash>` when `bundle`
 * is null, else in `bundles/<bundle>` from `offset`; `size` bytes either way. */
export interface StoredBlob {
  size: number;
  bundle: string | null;
  offset: number;
}

export class Index extends DurableObject<{ STORE: R2Bucket }> {
  private sql: SqlStorage;

  constructor(ctx: DurableObjectState, env: { STORE: R2Bucket }) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(`
      CREATE TABLE IF NOT EXISTS keys (key TEXT PRIMARY KEY, time INTEGER NOT NULL, changed INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS keys_changed ON keys (changed);
      CREATE INDEX IF NOT EXISTS keys_time ON keys (time);
      CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, time INTEGER NOT NULL, members TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS sequence (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS blobs (hash TEXT PRIMARY KEY, size INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS listing (name TEXT PRIMARY KEY, value TEXT NOT NULL);
    `);
    // Bundles came later than the table.
    const columns = this.sql
      .exec<{ name: string }>("PRAGMA table_info(blobs)")
      .toArray()
      .map((column) => column.name);
    if (!columns.includes("bundle")) {
      this.sql.exec("ALTER TABLE blobs ADD COLUMN bundle TEXT");
    }
    if (!columns.includes("offset")) {
      this.sql.exec("ALTER TABLE blobs ADD COLUMN offset INTEGER");
    }
    ctx.blockConcurrencyWhile(async () => {
      if (!this.listed() && (await ctx.storage.getAlarm()) === null) {
        await ctx.storage.setAlarm(Date.now());
      }
    });
  }

  /** Notes blobs just stored apart, by hash with their stored sizes. */
  noteBlobs(blobs: [string, number][]): void {
    this.note(blobs, "REPLACE");
  }

  /** Notes blobs found stored apart in the bucket. A size an upload noted
   * meanwhile is newer than what the bucket said, so it is kept. */
  noteFound(blobs: [string, number][]): void {
    this.note(blobs, "IGNORE");
  }

  private note(blobs: [string, number][], conflict: "REPLACE" | "IGNORE"): void {
    this.ctx.storage.transactionSync(() => {
      for (const [hash, size] of blobs) {
        this.sql.exec(`INSERT OR ${conflict} INTO blobs (hash, size) VALUES (?, ?)`, hash, size);
      }
    });
  }

  /** Notes blobs just stored in `bundle`, by hash with the length of their
   * frames and where those start in it. Reads take them from there from now
   * on, a blob also stored on its own included; one already in a bundle stays
   * where it is, so that sending a bundle again changes nothing. */
  noteBundled(bundle: string, blobs: [string, number, number][]): void {
    this.ctx.storage.transactionSync(() => {
      for (const [hash, size, offset] of blobs) {
        this.sql.exec(
          `INSERT INTO blobs (hash, size, bundle, offset) VALUES (?, ?, ?, ?)
           ON CONFLICT (hash) DO UPDATE SET size = excluded.size, bundle = excluded.bundle, offset = excluded.offset
           WHERE blobs.bundle IS NULL`,
          hash,
          size,
          bundle,
          offset,
        );
      }
    });
  }

  /** Where each of `hashes` stored apart is, or `null` for one that is not;
   * and whether that is known for all of them, which it is once the blobs
   * stored before the index held them have been listed. */
  blobsStored(hashes: string[]): { stored: (StoredBlob | null)[]; complete: boolean } {
    const found = new Map<string, StoredBlob>();
    for (let start = 0; start < hashes.length; start += BOUND) {
      const chunk = hashes.slice(start, start + BOUND);
      const rows = this.sql.exec<{ hash: string; size: number; bundle: string | null; offset: number | null }>(
        `SELECT hash, size, bundle, offset FROM blobs WHERE hash IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk,
      );
      for (const { hash, size, bundle, offset } of rows) {
        found.set(hash, { size, bundle, offset: offset ?? 0 });
      }
    }
    return { stored: hashes.map((hash) => found.get(hash) ?? null), complete: this.listed() };
  }

  /** Lists the bucket's blobs into the index, some pages at a time, until
   * every blob stored before the index held them is in it. A listing that
   * fails is tried again later from the last page listed, rather than left to
   * the runtime's few retries of a failed alarm. */
  async alarm(): Promise<void> {
    try {
      await this.listBucket();
    } catch (error) {
      console.error("listing the bucket's blobs failed; trying again later", error);
      await this.ctx.storage.setAlarm(Date.now() + RELIST_AFTER);
    }
  }

  private async listBucket(): Promise<void> {
    if (this.listed()) {
      return;
    }
    let cursor = this.listingCursor();
    for (let page = 0; page < LISTINGS_PER_ALARM; page++) {
      const listing = await this.env.STORE.list({ prefix: "blobs/", limit: 1000, cursor });
      this.noteFound(listing.objects.map((object) => [object.key.slice("blobs/".length), object.size]));
      if (!listing.truncated) {
        this.sql.exec("INSERT OR REPLACE INTO listing (name, value) VALUES ('done', '1')");
        return;
      }
      cursor = listing.cursor;
      this.sql.exec("INSERT OR REPLACE INTO listing (name, value) VALUES ('cursor', ?)", cursor);
    }
    await this.ctx.storage.setAlarm(Date.now());
  }

  private listed(): boolean {
    return this.sql.exec("SELECT 1 FROM listing WHERE name = 'done'").toArray().length > 0;
  }

  private listingCursor(): string | undefined {
    const row = this.sql.exec<{ value: string }>("SELECT value FROM listing WHERE name = 'cursor'").toArray()[0];
    return row?.value;
  }

  /** Lists `key` as committed at `time`, or keeps its later time. */
  commit(key: string, time: number): void {
    this.ctx.storage.transactionSync(() => {
      const changed = this.next();
      this.sql.exec(
        `INSERT INTO keys (key, time, changed) VALUES (?, ?, ?)
         ON CONFLICT (key) DO UPDATE SET time = excluded.time, changed = excluded.changed
         WHERE excluded.time > keys.time`,
        key,
        time,
        changed,
      );
      this.expire();
    });
  }

  /** The listing of the keys changed after `since` (all keys when `null`)
   * and of every run, with the cursor that names this point. */
  list(since: number | null): { text: string; cursor: string } {
    const lines: string[] = [];
    const keys = this.sql.exec<{ key: string; time: number }>(
      "SELECT key, time FROM keys WHERE changed > ? ORDER BY key",
      since ?? -1,
    );
    for (const { key, time } of keys) {
      lines.push(`${key} ${time}\n`);
    }
    const runs = this.sql.exec<{ id: string; time: number; members: string }>(
      "SELECT id, time, members FROM runs ORDER BY time DESC, id DESC",
    );
    for (const { id, time, members } of runs) {
      lines.push(`run ${id} ${time}${members ? ` ${members}` : ""}\n`);
    }
    return { text: lines.join(""), cursor: String(this.current()) };
  }

  /** Takes a run's listing: renews the times of the keys it names that the
   * index holds, remembers its runs, and trims the history. Lines that are
   * neither are left out. */
  record(listing: string, now: number): void {
    const keys: [string, number][] = [];
    const runs: [string, number, string][] = [];
    for (const line of listing.split("\n")) {
      const words = line.trimEnd().split(" ");
      if (words[0] === "run") {
        const [, id, time, ...members] = words;
        if (id && time && TIME.test(time)) {
          const named = members.filter((member) => MEMBER.test(member)).slice(0, RUN_MEMBERS);
          runs.push([id, Number(time), named.join(" ")]);
        }
      } else if (words.length === 2 && KEY.test(words[0]!) && TIME.test(words[1]!)) {
        keys.push([words[0]!, Number(words[1])]);
      }
    }
    this.ctx.storage.transactionSync(() => {
      const changed = this.next();
      for (const [key, time] of keys) {
        this.sql.exec("UPDATE keys SET time = ?, changed = ? WHERE key = ? AND time < ?", time, changed, key, time);
      }
      for (const [id, time, members] of runs) {
        this.sql.exec("INSERT OR REPLACE INTO runs (id, time, members) VALUES (?, ?, ?)", id, time, members);
      }
      this.trim(now);
      this.expire();
    });
  }

  /** Keeps the most recent runs within the history's bounds, as the client's
   * `Known::expire` does. */
  private trim(now: number): void {
    this.sql.exec("DELETE FROM runs WHERE time < ?", now - HISTORY_FOR);
    const runs = this.sql.exec<{ id: string; members: string }>(
      "SELECT id, members FROM runs ORDER BY time DESC, id DESC",
    );
    const dropped: string[] = [];
    let kept = 0;
    let members = 0;
    for (const run of runs) {
      members += run.members ? (run.members.length + 1) / 17 : 0;
      if (kept < HISTORY_RUNS && members <= HISTORY_MEMBERS) {
        kept++;
      } else {
        dropped.push(run.id);
        members = Infinity;
      }
    }
    for (const id of dropped) {
      this.sql.exec("DELETE FROM runs WHERE id = ?", id);
    }
  }

  private expire(): void {
    this.sql.exec("DELETE FROM keys WHERE time < ?", Date.now() - KEPT_FOR);
  }

  private current(): number {
    const row = this.sql.exec<{ value: number }>("SELECT value FROM sequence WHERE name = 'changed'").toArray()[0];
    return row?.value ?? 0;
  }

  private next(): number {
    const value = this.current() + 1;
    this.sql.exec("INSERT OR REPLACE INTO sequence (name, value) VALUES ('changed', ?)", value);
    return value;
  }
}
