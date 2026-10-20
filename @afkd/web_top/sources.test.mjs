// The afkd source inventory's own suite: `node --test @afkd/web_top/sources.test.mjs`.
//
// The transcription tests read afkd's Rust by path, out of the checkout `AFKD_SRC` names, and a
// path is the one thing about that Rust they never check. When afkd folded `crates/config` into
// `crates/lang` (6150a462), `keymap.test.mjs`'s read became a bare ENOENT at module load: the
// suite failed as one nameless error and every transcription check in it was lost. This suite
// names the moved file instead. `AFKD_SOURCES` in `testkit.mjs` lists every afkd file a suite
// reads, `afkdSource` refuses a path not on it, and each one below is its own test, so a file
// afkd moves is a failing test with the path as its name.
//
// The inventory is held to the suites too, with no checkout needed: a scan of every sibling
// suite's `afkdSource(...)` calls must find exactly the listed paths, so a read added without
// listing it, or an entry left behind after its read is gone, is red even where the release
// workflow runs.

import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

import { AFKD_SOURCES, AFKD_SRC, NO_AFKD_SRC } from "./testkit.mjs";

const HERE = import.meta.dirname;

for (const path of AFKD_SOURCES) {
  test(`afkd still has ${path}`, { skip: NO_AFKD_SRC }, () => {
    assert.ok(
      existsSync(join(AFKD_SRC, path)),
      `${path} is not in the afkd checkout AFKD_SRC names: it moved, so the suite that reads it is red`,
    );
  });
}

test("the inventory lists every afkd file a suite reads", () => {
  const read = new Set();
  for (const file of readdirSync(HERE).filter((name) => name.endsWith(".test.mjs"))) {
    for (const [, args] of readFileSync(join(HERE, file), "utf8").matchAll(/afkdSource\(((?:\s*"[^"]*"\s*,?)+)\)/g)) {
      read.add([...args.matchAll(/"([^"]*)"/g)].map(([, part]) => part).join("/"));
    }
  }
  // Ten calls over eight files today: a regex that rots finds none, and an empty set must not
  // pass for an empty inventory.
  assert.ok(read.size >= 8, `the scan found only ${read.size} afkd paths read: the afkdSource regex no longer matches the calls`);
  assert.deepEqual([...read].sort(), [...AFKD_SOURCES].sort());
});
