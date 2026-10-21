import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

test("extractor preserves Fig generator cache units and policies", () => {
  const testDir = dirname(fileURLToPath(import.meta.url));
  const out = mkdtempSync(join(tmpdir(), "insh-extractor-cache-"));
  try {
    const run = spawnSync(process.execPath, ["--import", "tsx", "extract.ts"], {
      cwd: join(testDir, ".."),
      env: { ...process.env, SRC: join(testDir, "fixtures"), OUT: out },
      encoding: "utf8",
      timeout: 30_000,
    });
    assert.equal(run.status, 0, run.stderr);
    const spec = JSON.parse(readFileSync(join(out, "extras", "cache.json"), "utf8"));
    const generators = spec.args[0].generators;
    assert.equal(generators.length, 5);
    assert.deepEqual(generators.map((g: any) => g.cache), [
      { ttl_ms: 1500, by_dir: true, strategy: "max_age" },
      { ttl_ms: 30000, by_dir: false, strategy: "stale_while_revalidate" },
      undefined,
      { ttl_secs: 2, by_dir: true, strategy: "max_age" },
      { ttl_ms: 0 },
    ]);
    assert.equal(Object.hasOwn(generators[2], "cache"), false);
    assert.deepEqual(generators[0].input, { kind: "shell", script: "printf branches" });
    assert.deepEqual(generators[1].input, { kind: "argv", argv: ["printf", "packages"] });
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
});
