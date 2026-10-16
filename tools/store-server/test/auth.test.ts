import { describe, expect, it } from "vitest";
import { READ, WRITE, blob, hash, putApart, record, request, upload } from "./helpers";

describe("tokens", () => {
  it("are required", async () => {
    const response = await request("HEAD", `entries/${hash()}`, { token: null });
    expect(response.status).toBe(401);
    const json = await (await request("GET", "index", { token: null })).json();
    expect(json).toMatchObject({ error: "unauthorized" });
  });

  it("must be known", async () => {
    const response = await request("GET", "index", { token: "not-a-token" });
    expect(response.status).toBe(401);
  });

  it("let a read token read but not write", async () => {
    expect((await request("HEAD", `entries/${hash()}`, { token: READ })).status).toBe(404);
    const key = hash();
    const response = await request("PUT", `entries/${key}`, { token: READ, body: upload(record(key, []), []) });
    expect(response.status).toBe(403);
    expect(await response.json()).toMatchObject({ error: "read_only" });
    expect((await request("POST", "runs", { token: READ, body: "" })).status).toBe(403);
  });

  it("let a write token write and read", async () => {
    const key = hash();
    const response = await request("PUT", `entries/${key}`, { token: WRITE, body: upload(record(key, []), []) });
    expect(response.status).toBe(204);
    expect((await request("HEAD", `entries/${key}`, { token: WRITE })).status).toBe(200);
    expect((await request("HEAD", `entries/${key}`, { token: READ })).status).toBe(200);
    const stored = blob("apart", false);
    await putApart(stored);
    expect((await request("HEAD", `blobs/${stored.hash}`, { token: READ })).status).toBe(200);
  });
});
