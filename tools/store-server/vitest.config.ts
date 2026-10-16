import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      miniflare: {
        bindings: {
          // SHA-256 digests of "read-token" and "write-token", the tokens
          // test/helpers.ts sends.
          READ_TOKENS: "0328587016f9e316b9c31c94f944a2c453e1494a564062b5eab68039c75b58bf",
          WRITE_TOKENS: "ab893cb32670dc9e3bf2563af8d7487b88c279c5850c76f35d6eafa8fa34acf2",
        },
      },
    }),
  ],
});
