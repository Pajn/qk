// The index: which entries the store holds, with when a run last found each,
// and the runs that used them. One Durable Object holds it all in SQLite, so
// every update is ordered and a cursor names a point in that order. The line
// formats are those of crates/qk-cache/src/remote/index.rs.

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

export class Index extends DurableObject<unknown> {
  private sql: SqlStorage;

  constructor(ctx: DurableObjectState, env: unknown) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(`
      CREATE TABLE IF NOT EXISTS keys (key TEXT PRIMARY KEY, time INTEGER NOT NULL, changed INTEGER NOT NULL);
      CREATE INDEX IF NOT EXISTS keys_changed ON keys (changed);
      CREATE INDEX IF NOT EXISTS keys_time ON keys (time);
      CREATE TABLE IF NOT EXISTS runs (id TEXT PRIMARY KEY, time INTEGER NOT NULL, members TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS sequence (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
    `);
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
