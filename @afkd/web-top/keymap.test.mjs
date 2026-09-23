// The keymap transcription's own suite: `node --test @afkd/web-top/keymap.test.mjs`.
//
// This is the pinning test the card asks for. `keymap.mjs` is a hand-written copy of afkd's
// `DEFAULT_KEYS`, and a hand-written copy of a table in another language is exactly the kind of
// thing that rots — so every assertion below reads the **Rust** and holds the javascript to it:
// the 51 rows in order, the eight scopes and their dotted names, the glyph vocabulary, and the
// drain's refusal set. A row gained, lost or rebound over there is a failing test here, which is
// the only thing that makes "the stock keymap, transcribed" a claim rather than a hope. The Rust
// is read out of the afkd checkout `AFKD_SRC` names, and those tests skip without one.
//
// Each read is guarded the way `web/scripts/check-dist.mjs` guards its palette read: if the
// regex finds implausibly little, the test fails *with that* as its message, so a renamed
// constant reddens rather than silently comparing against an empty list.

import assert from "node:assert/strict";
import test from "node:test";

import {
  ACTIONS,
  DEFAULT_KEYS,
  DESCRIPTIONS,
  HANDLED,
  NOTES,
  PARTIAL,
  REFUSED_WHILE_QUITTING,
  SCOPES,
  all,
  chords,
  glyph,
  glyphs,
  idOf,
  primary,
  resolve,
} from "./keymap.mjs";
import { NO_AFKD_SRC, afkdSource } from "./testkit.mjs";

const KEYMAP_RS = NO_AFKD_SRC ? "" : afkdSource("crates", "config", "src", "keymap.rs");
const KEYS_RS = NO_AFKD_SRC ? "" : afkdSource("crates", "tui", "src", "keys.rs");

// --- AC1: the transcription is the Rust table ---------------------------------------

test("the 51 rows are DEFAULT_KEYS's, in order", { skip: NO_AFKD_SRC }, () => {
  // The slice itself, bounded so a `(Scope::…, "…", "…")` tuple elsewhere in the file cannot
  // drift into the comparison.
  const from = KEYMAP_RS.indexOf("const DEFAULT_KEYS: &[(Scope, &str, &str)] = &[");
  assert.notEqual(from, -1, "keymap.rs still declares DEFAULT_KEYS as a slice of triples");
  const slice = KEYMAP_RS.slice(from, KEYMAP_RS.indexOf("\n];", from));
  const rust = [...slice.matchAll(/\(Scope::(\w+),\s*"([\w_]+)",\s*"([^"]*)"\)/g)].map(
    ([, scope, action, binding]) => ({ scope, action, binding }),
  );
  assert.ok(
    rust.length >= 51,
    `the DEFAULT_KEYS read found only ${rust.length} rows — the table's shape moved, so this test is comparing against a stub`,
  );

  // `Scope::OutputTree` → `output.tree`, read off `dsl_name` rather than transcribed, so a
  // scope renamed there fails here too.
  const dsl = Object.fromEntries(
    [...KEYMAP_RS.matchAll(/Scope::(\w+) => "([\w.]+)",/g)].map(([, variant, name]) => [variant, name]),
  );
  assert.equal(Object.keys(dsl).length, 8, "dsl_name still spells eight scopes");

  assert.deepEqual(
    DEFAULT_KEYS,
    rust.map(({ scope, action, binding }) => ({ scope: dsl[scope], action, binding })),
    "keymap.mjs's transcription is DEFAULT_KEYS row for row, scope for scope, binding for binding",
  );
  assert.equal(DEFAULT_KEYS.length, 51, "…and it is still 51 rows");
});

test("the scopes are Scope's, in its own order", { skip: NO_AFKD_SRC }, () => {
  const arms = [...KEYMAP_RS.matchAll(/Scope::(\w+) => "([\w.]+)",/g)].map(([, , name]) => name);
  assert.equal(arms.length, 8, "the dsl_name read found the eight arms");
  assert.deepEqual(SCOPES, arms, "SCOPES is dsl_name's list in its own order");
  // …and every row's scope is one of them, so a typo cannot mint a ninth.
  for (const row of DEFAULT_KEYS) {
    assert.ok(SCOPES.includes(row.scope), `${row.scope} is a scope`);
  }
  // The counts the card states, per scope — a row moving between two scopes without changing
  // the total would otherwise pass the deep-equal above only because it moved in both files.
  const counts = SCOPES.map((scope) => DEFAULT_KEYS.filter((r) => r.scope === scope).length);
  assert.deepEqual(counts, [3, 21, 2, 3, 10, 9, 1, 2], "3 / 21 / 2 / 3 / 10 / 9 / 1 / 2");
});

test("the alternates are transcribed, not collapsed to the first", () => {
  // The two the card names by hand, because an alternate is the easiest thing to lose in a
  // transcription: nothing on screen shows it and the primary keeps working.
  assert.deepEqual(chords("global.quit"), [{ ctrl: false, key: "q" }, { ctrl: true, key: "c" }]);
  assert.deepEqual(chords("confirm.cancel"), [{ ctrl: false, key: "n" }, { ctrl: false, key: "esc" }]);
  // …and every multi-atom row in the Rust is multi-chord here, so the rule holds over all of
  // them rather than over the two that were written down.
  for (const row of DEFAULT_KEYS) {
    assert.equal(
      chords(idOf(row)).length,
      row.binding.split(" ").length,
      `${idOf(row)} parses one chord per atom of ${JSON.stringify(row.binding)}`,
    );
  }
});

// --- the glyph vocabulary ------------------------------------------------------------

test("the glyphs are keys::glyph's", { skip: NO_AFKD_SRC }, () => {
  // `named_glyph`'s arms, read out of the Rust and keyed by the token `NamedKey::to_token`
  // spells — so both halves of the pair come from the file rather than from this test.
  const token = Object.fromEntries(
    [...KEYMAP_RS.matchAll(/NamedKey::(\w+) => "([a-z]+)",/g)].map(([, variant, name]) => [variant, name]),
  );
  const rust = [...KEYS_RS.matchAll(/NamedKey::(\w+) => "([^"]+)",/g)].map(([, variant, spelling]) => [
    token[variant],
    spelling,
  ]);
  assert.equal(rust.length, 11, `named_glyph still spells eleven keys, not ${rust.length}`);
  for (const [name, spelling] of rust) {
    assert.equal(glyph({ ctrl: false, key: name }), spelling, `${name} spells ${spelling}`);
  }
  // The two composition rules beside the table: a `ctrl-` chord uppercases its letter (lossless
  // — the grammar admits only the lowercase spelling), a bare char keeps its case because the
  // case is semantic.
  assert.equal(glyph({ ctrl: true, key: "r" }), "Ctrl+R");
  assert.equal(glyph({ ctrl: true, key: "u" }), "Ctrl+U");
  assert.equal(glyph({ ctrl: false, key: "g" }), "g");
  assert.equal(glyph({ ctrl: false, key: "G" }), "G");
  assert.equal(glyph({ ctrl: false, key: "?" }), "?");
  // A named key under ctrl keeps its Title-case spelling inside the `Ctrl+` frame.
  assert.equal(glyph({ ctrl: true, key: "enter" }), "Ctrl+Enter");
  // `primary` is the first chord alone (the alias is an escape hatch, not an advertisement);
  // `all` teaches both spellings; `glyphs` is the array the footer reads.
  assert.equal(primary("global.quit"), "q");
  assert.equal(all("global.quit"), "q/Ctrl+C");
  assert.deepEqual(glyphs("overview.group_collapse"), ["h", "←"]);
  assert.equal(all("overview.service_peek"), "Enter/Space");
  assert.equal(primary("nope.not_an_action"), null, "an unknown action is unbound, not a throw");
  assert.equal(all("nope.not_an_action"), null);
});

// --- scope resolution is precedence, not union ---------------------------------------

test("a lane row's `queues` shadows the overview's group fold", () => {
  const h = { ctrl: false, key: "h" };
  const left = { ctrl: false, key: "left" };
  // On a lane row the `queues` scope is consulted **before** `overview`, which it shadows —
  // the whole of `Ctx::Queues`'s documented reason for existing.
  assert.equal(resolve(["global", "queues", "overview"], h), "queues.narrow");
  assert.equal(resolve(["global", "queues", "overview"], left), "queues.narrow");
  // Anywhere else the same key is the group fold. Same chord, same table, different row.
  assert.equal(resolve(["global", "overview"], h), "overview.group_collapse");
  assert.equal(resolve(["global", "overview"], left), "overview.group_collapse");
  // `global` is first from both, so a lane row cannot shadow quit or reload.
  for (const scopes of [["global", "queues", "overview"], ["global", "overview"]]) {
    assert.equal(resolve(scopes, { ctrl: false, key: "q" }), "global.quit");
    assert.equal(resolve(scopes, { ctrl: true, key: "c" }), "global.quit");
    assert.equal(resolve(scopes, { ctrl: true, key: "r" }), "global.reload");
  }
  // A scope not in the list binds nothing, however bound the chord is elsewhere: `f` is
  // `output.tree.follow` and `output.log.follow`, and neither is reachable from the list.
  assert.equal(resolve(["global", "overview"], { ctrl: false, key: "f" }), null);
  assert.equal(resolve(["output.tree"], { ctrl: false, key: "f" }), "output.tree.follow");
  // An unbound chord resolves to nothing rather than to the first row.
  assert.equal(resolve(["global", "queues", "overview"], { ctrl: false, key: "ø" }), null);
  assert.equal(resolve([], { ctrl: false, key: "q" }), null);
});

// --- every row is described, and handled or noted -------------------------------------

test("every action is described, and either handled or noted", () => {
  assert.equal(ACTIONS.length, 51);
  assert.equal(new Set(ACTIONS).size, 51, "no two rows share a dotted id");
  for (const id of ACTIONS) {
    const description = DESCRIPTIONS[id];
    assert.equal(typeof description, "string", `${id} has a description`);
    assert.notEqual(description, "", `${id}'s description is not empty`);
    // Sentence case: a leading capital, and no Title Case run — `docs/tui-style.md` §8's rule
    // for a label, which the overlay is full of.
    assert.match(description, /^[A-Z]/, `${id}'s description is Sentence case: ${description}`);
    assert.ok(HANDLED.has(id) || NOTES[id] !== undefined, `${id} is handled or carries a note`);
  }
  // Nothing in either table names a row that is not in the keymap — a note for a deleted
  // action would outlive the action and read as an explanation of nothing.
  for (const id of [...HANDLED, ...Object.keys(NOTES), ...Object.keys(DESCRIPTIONS)]) {
    assert.ok(ACTIONS.includes(id), `${id} is a row of the table`);
  }
  // The deliberately partial actions, named rather than inferred: `Enter`/`Space` folds a group
  // header and does nothing on a service row, and `o`/`i` open a **service**'s run view and info
  // page and have no subject on a group header or a lane row.
  assert.deepEqual(
    ACTIONS.filter((id) => HANDLED.has(id) && NOTES[id] !== undefined),
    PARTIAL,
    "only the declared PARTIAL actions are both handled and noted",
  );
  // Pinned in `ACTIONS` **table order**, which is what `keymap.mjs`'s own module-load check
  // compares against: an appended entry throws on import.
  assert.deepEqual(PARTIAL, ["overview.service_peek", "overview.show_output", "overview.show_info"]);
});

test("the drain refuses exactly the actions keys.rs refuses", { skip: NO_AFKD_SRC }, () => {
  // `keys::REFUSED_WHILE_QUITTING` is a list of `act::` constants; each constant's value is the
  // scope-qualified action name, so the two are joined through the `act` module's own
  // definitions rather than through a second transcription.
  const from = KEYS_RS.indexOf("pub(crate) const REFUSED_WHILE_QUITTING");
  assert.notEqual(from, -1, "keys.rs still declares REFUSED_WHILE_QUITTING");
  const names = [...KEYS_RS.slice(from, KEYS_RS.indexOf("];", from)).matchAll(/act::(\w+),/g)].map(
    ([, name]) => name,
  );
  assert.equal(names.length, 9, `the refusal read found ${names.length} constants, not nine`);
  // `pub(crate) const LIST_START: Action = (Scope::Overview, "service_start");`
  const dsl = Object.fromEntries(
    [...KEYMAP_RS.matchAll(/Scope::(\w+) => "([\w.]+)",/g)].map(([, variant, name]) => [variant, name]),
  );
  const rust = names.map((name) => {
    const decl = KEYS_RS.match(new RegExp(`const ${name}: Action = \\(Scope::(\\w+), "([\\w_]+)"\\);`));
    assert.notEqual(decl, null, `keys.rs declares act::${name}`);
    return `${dsl[decl[1]]}.${decl[2]}`;
  });
  assert.deepEqual(
    [...REFUSED_WHILE_QUITTING].sort(),
    [...rust].sort(),
    "the drain's refusal set is keys.rs's, so the footer and the dispatch refuse one set",
  );
  // …and every one of them is a real row, so a drain cannot refuse an action nothing binds.
  for (const id of REFUSED_WHILE_QUITTING) assert.ok(ACTIONS.includes(id), `${id} is a row`);
});
