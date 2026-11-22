// The index: which entries the store holds, with when a run last found each,
// and the runs that used them. One Durable Object holds it all in SQLite, so
// every update is ordered and a cursor names a point in that order. The line
// formats are those of crates/qk-cache/src/remote/index.rs.
//
// It also holds which blobs are stored apart, with their sizes, so that a
// record citing thousands of them is checked and composed with a few queries
// rather than a bucket request per blob. Blobs stored before it held them are
// listed from the bucket once, in the background.
//
// And it holds the records that cite no blob, path sets and expectations,
// beside their copies in the bucket: every run reads them and few write them,
// and a read of a small object from the bucket costs a request each that the
// Worker waits on, where the index answers many from memory in one. A record
// too large for a row is listed without its body and read from the bucket.
// Each save replaces the row as it replaces the object, so a read finds the
// last save that succeeded, never an older one. Records saved before the
// index held them are listed from the bucket once, in the background, like
// blobs, and their bodies taken in as they are read.

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
/** What the alarm lists from the bucket, in order, each once: blobs stored
 * apart, then records citing no blob. Each is done once its name is in the
 * `listing` table as done. */
const LISTINGS = [
  { prefix: "blobs/", done: "done", cursor: "cursor" },
  { prefix: "pathsets/", done: "pathsets done", cursor: "pathsets cursor" },
  { prefix: "expectations/", done: "expectations done", cursor: "expectations cursor" },
] as const;
/** The largest record held with its body; a row holds 2 MB at most. */
export const HELD_RECORD = 1 << 20;
/** Bytes of records' bodies one read answers at most, within what one call to
 * a Durable Object returns; past them a record is answered as stored in the
 * bucket. */
const RECORD_ANSWER = 16 << 20;
/** How long a record's body is held after it was last read or saved. Past
 * that it is dropped to keep the index small, and read from the bucket again
 * when next asked for. */
const RECORD_HELD_FOR = 30 * DAY;
/** Bytes of records' bodies on their way to the bucket through the index at
 * once, well within the memory a Durable Object has. A save past them is
 * refused before its body is sent here. */
export const SAVING_AT_ONCE = 48 << 20;
/** How long room admitted for a save is kept while the save has not come. */
const ADMITTED_FOR = 60_000;

/** What the index says of a record citing no blob, named as its object in the
 * bucket: `held`, with its body; `apart`, stored in the bucket alone, or held
 * past what one answer carries, with the version of its row; `none`, not
 * stored; or `unknown`, not listed yet, so the bucket must be asked. */
export type RecordState =
  | { state: "held"; body: Uint8Array }
  | { state: "apart"; version: number }
  | { state: "none" }
  | { state: "unknown" };

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
      CREATE TABLE IF NOT EXISTS records (name TEXT PRIMARY KEY, body BLOB, version INTEGER NOT NULL, used INTEGER NOT NULL, saving INTEGER NOT NULL DEFAULT 0);
      CREATE INDEX IF NOT EXISTS records_used ON records (used);
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
      if (LISTINGS.some((listing) => !this.done(listing.done)) && (await ctx.storage.getAlarm()) === null) {
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

  /** Lists the bucket's blobs, then its records citing no blob, into the
   * index, some pages at a time, until everything stored before the index
   * held it is in it. A listing that fails is tried again later from the last
   * page listed, rather than left to the runtime's few retries of a failed
   * alarm. */
  async alarm(): Promise<void> {
    try {
      await this.listBucket();
    } catch (error) {
      console.error("listing the bucket failed; trying again later", error);
      await this.ctx.storage.setAlarm(Date.now() + RELIST_AFTER);
    }
  }

  private async listBucket(): Promise<void> {
    const pending = LISTINGS.find((listing) => !this.done(listing.done));
    if (!pending) {
      return;
    }
    let cursor = this.listingValue(pending.cursor);
    for (let page = 0; page < LISTINGS_PER_ALARM; page++) {
      const listing = await this.env.STORE.list({ prefix: pending.prefix, limit: 1000, cursor });
      if (pending.prefix === "blobs/") {
        this.noteFound(listing.objects.map((object) => [object.key.slice("blobs/".length), object.size]));
      } else {
        this.noteRecords(listing.objects.map((object) => object.key));
      }
      if (!listing.truncated) {
        this.sql.exec("INSERT OR REPLACE INTO listing (name, value) VALUES (?, '1')", pending.done);
        break;
      }
      cursor = listing.cursor;
      this.sql.exec("INSERT OR REPLACE INTO listing (name, value) VALUES (?, ?)", pending.cursor, cursor);
    }
    await this.ctx.storage.setAlarm(Date.now());
  }

  /** Whether the blobs stored before the index held them are listed. */
  private listed(): boolean {
    return this.done("done");
  }

  private done(name: string): boolean {
    return this.sql.exec("SELECT 1 FROM listing WHERE name = ?", name).toArray().length > 0;
  }

  private listingValue(name: string): string | undefined {
    const row = this.sql.exec<{ value: string }>("SELECT value FROM listing WHERE name = ?", name).toArray()[0];
    return row?.value;
  }

  /** Whether the records under `name`'s prefix that were saved before the
   * index held them are listed, so that one it lacks is not stored. */
  private recordsListed(name: string): boolean {
    const listing = LISTINGS.find((listing) => listing.prefix !== "blobs/" && name.startsWith(listing.prefix));
    return listing !== undefined && this.done(listing.done);
  }

  /** Notes records found in the bucket, without their bodies, which are
   * taken in as they are read. A save noted meanwhile is kept. */
  private noteRecords(names: string[]): void {
    const now = Date.now();
    this.ctx.storage.transactionSync(() => {
      for (const name of names) {
        this.sql.exec(
          "INSERT OR IGNORE INTO records (name, body, version, used) VALUES (?, NULL, ?, ?)",
          name,
          this.nextVersion(),
          now,
        );
      }
    });
  }

  /** Saves of records citing no blob that have not ended, by name. */
  private saves = new Map<string, Promise<void>>();

  /** Room admitted for saves by `admitSave`, by ticket: its bytes, and when
   * it is given up unless the save has come by then. */
  private admitted = new Map<string, { bytes: number; until: number }>();

  /** Admits a save of `bytes` bytes, answering the ticket `saveRecord`
   * takes, or null while the saves admitted hold `SAVING_AT_ONCE` bytes
   * between them. Asked before a body is sent here, so the bodies waiting
   * for the bucket, of which one name's wait for each other, never take more
   * memory than that. One save is always admitted when no other is. */
  admitSave(bytes: number): string | null {
    const now = Date.now();
    let held = 0;
    for (const [ticket, room] of this.admitted) {
      if (room.until < now) {
        this.admitted.delete(ticket);
      } else {
        held += room.bytes;
      }
    }
    if (held > 0 && held + bytes > SAVING_AT_ONCE) {
      return null;
    }
    const ticket = crypto.randomUUID();
    this.admitted.set(ticket, { bytes, until: now + ADMITTED_FOR });
    return ticket;
  }

  /** Saves `body` as the record `name`, which cites no blob: in the bucket,
   * and here with its body when that is at most `HELD_RECORD` bytes. Bodies
   * unread for `RECORD_HELD_FOR` are dropped on the way. False, with nothing
   * saved, when `ticket` is not one `admitSave` gave for a body this large
   * and still keeps.
   *
   * The index writes the bucket itself, one save of a name after another,
   * so the save the bucket keeps is the one the index keeps. While a save
   * is on its way the index holds no body for the name and takes none in,
   * so reads go to the bucket, and a save that never ends leaves them
   * there until the next one. */
  async saveRecord(name: string, body: Uint8Array, ticket: string): Promise<boolean> {
    const room = this.admitted.get(ticket);
    if (!room || room.bytes < body.length || room.until === Infinity) {
      return false;
    }
    // Kept until the save has ended, however long it waits its turn.
    room.until = Infinity;
    const before = this.saves.get(name);
    const save = (async () => {
      // Whatever became of the save before it, this one follows it.
      await before?.catch(() => {});
      this.sql.exec(
        `INSERT INTO records (name, body, version, used, saving) VALUES (?, NULL, ?, ?, 1)
         ON CONFLICT (name) DO UPDATE SET body = NULL, version = excluded.version, saving = 1`,
        name,
        this.nextVersion(),
        Date.now(),
      );
      await this.env.STORE.put(name, body, { httpMetadata: { contentType: "application/x-qk-upload" } });
      const now = Date.now();
      this.ctx.storage.transactionSync(() => {
        this.sql.exec(
          "INSERT OR REPLACE INTO records (name, body, version, used, saving) VALUES (?, ?, ?, ?, 0)",
          name,
          body.length <= HELD_RECORD ? buffer(body) : null,
          this.nextVersion(),
          now,
        );
        this.sql.exec("UPDATE records SET body = NULL WHERE used < ? AND body IS NOT NULL", now - RECORD_HELD_FOR);
      });
    })();
    this.saves.set(name, save);
    try {
      await save;
      return true;
    } finally {
      this.admitted.delete(ticket);
      if (this.saves.get(name) === save) {
        this.saves.delete(name);
      }
    }
  }

  /** What the index says of each of `names`, records citing no blob, in
   * order. Bodies are answered while they total at most `RECORD_ANSWER`
   * bytes; past that a held record is answered as `apart`. */
  readRecords(names: string[]): RecordState[] {
    const now = Date.now();
    // Sizes before bodies, so that a request naming more held records than
    // one answer carries never has all their bodies in memory at once.
    const rows = new Map<string, { size: number | null; version: number; used: number }>();
    for (let start = 0; start < names.length; start += BOUND) {
      const chunk = names.slice(start, start + BOUND);
      const found = this.sql.exec<{ name: string; size: number | null; version: number; used: number }>(
        `SELECT name, length(body) AS size, version, used FROM records WHERE name IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk,
      );
      for (const { name, size, version, used } of found) {
        rows.set(name, { size, version, used });
      }
    }
    // Reading renews a body's hold, at most once a day, so that reads seldom
    // write.
    const renewed = [...rows].filter(([, row]) => row.size !== null && row.used < now - DAY).map(([name]) => name);
    for (let start = 0; start < renewed.length; start += BOUND - 1) {
      const chunk = renewed.slice(start, start + BOUND - 1);
      this.sql.exec(`UPDATE records SET used = ? WHERE name IN (${chunk.map(() => "?").join(", ")})`, now, ...chunk);
    }
    let answered = 0;
    const answer = new Set<string>();
    for (const name of names) {
      const size = rows.get(name)?.size;
      if (size != null && !answer.has(name) && answered + size <= RECORD_ANSWER) {
        answered += size;
        answer.add(name);
      }
    }
    // Nothing else runs between the two reads, so each body is the one whose
    // size was counted.
    const bodies = new Map<string, ArrayBuffer>();
    const fitting = [...answer];
    for (let start = 0; start < fitting.length; start += BOUND) {
      const chunk = fitting.slice(start, start + BOUND);
      const found = this.sql.exec<{ name: string; body: ArrayBuffer }>(
        `SELECT name, body FROM records WHERE name IN (${chunk.map(() => "?").join(", ")})`,
        ...chunk,
      );
      for (const { name, body } of found) {
        bodies.set(name, body);
      }
    }
    return names.map((name): RecordState => {
      const row = rows.get(name);
      if (!row) {
        return this.recordsListed(name) ? { state: "none" } : { state: "unknown" };
      }
      const body = bodies.get(name);
      return body ? { state: "held", body: new Uint8Array(body) } : { state: "apart", version: row.version };
    });
  }

  /** Takes in the body of the record `name`, as just read from the bucket,
   * unless the index has heard of a save since the read began, or one is on
   * its way: `version` is that of the row the read was answered `apart`
   * from, or null when the index had none. So a slow read of an older body
   * never replaces a newer save's. A record too large to hold is left as it
   * is. */
  fillRecord(name: string, version: number | null, body: Uint8Array): void {
    if (body.length > HELD_RECORD) {
      return;
    }
    const now = Date.now();
    if (version === null) {
      this.sql.exec(
        "INSERT OR IGNORE INTO records (name, body, version, used) VALUES (?, ?, ?, ?)",
        name,
        buffer(body),
        this.nextVersion(),
        now,
      );
    } else {
      this.sql.exec(
        "UPDATE records SET body = ?, used = ? WHERE name = ? AND version = ? AND body IS NULL AND saving = 0",
        buffer(body),
        now,
        name,
        version,
      );
    }
  }

  private nextVersion(): number {
    const row = this.sql.exec<{ value: number }>("SELECT value FROM sequence WHERE name = 'records'").toArray()[0];
    const value = (row?.value ?? 0) + 1;
    this.sql.exec("INSERT OR REPLACE INTO sequence (name, value) VALUES ('records', ?)", value);
    return value;
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

/** The bytes `view` covers, as SQLite binds a blob. */
function buffer(view: Uint8Array): ArrayBuffer {
  return view.slice().buffer;
}
