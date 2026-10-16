import type { Env as WorkerEnv } from "../src/worker";

declare global {
  namespace Cloudflare {
    interface Env extends WorkerEnv {}
  }
}
