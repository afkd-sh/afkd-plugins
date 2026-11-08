// The keymap transcription's own suite: `node --test @afkd/web_top/keymap.test.mjs`.
//
// This is the pinning test the card asks for. `keymap.mjs` is a hand-written copy of afkd's
// `DEFAULT_KEYS`, and a hand-written copy of a table in another language is exactly the kind of
// thing that rots — so every assertion below reads the **Rust** and holds the javascript to it:
// the 51 rows in order, the eight scopes in `Scope`'s order, the glyph vocabulary, and the
// drain's refusal set. A row gained, lost or changed over there is a failing test here, which is
// the only thing that makes "the stock keymap, transcribed" a claim rather than a hope. The Rust
// is read out of the afkd checkout `AFKD_SRC` names, and those tests skip without one.
//
// The dotted scope names (`output.tree`) and the bare key words are this page's own vocabulary,
// since afkd top names neither: a scope's name comes from its `Scope` variant by `dotted`'s rule,
// and a key's word is the one `NamedKey::from_token` reads, so both stay pinned to the Rust.
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

const KEYMAP_RS = NO_AFKD_SRC ? "" : afkdSource("crates", "lang", "src", "keymap.rs");
const KEYS_RS = NO_AFKD_SRC ? "" : afkdSource("crates", "tui", "src", "keys.rs");
const HELP_RS = NO_AFKD_SRC ? "" : afkdSource("crates", "tui", "src", "help.rs");

/// The source text of the method whose signature starts `signature`, up to its closing brace at
/// method indent: the bound that keeps one impl's `Self::` arms out of another's read.
function fnBody(source, signature) {
  const at = source.indexOf(signature);
  assert.notEqual(at, -1, `keymap.rs still has ${signature}`);
  return source.slice(at, source.indexOf("\n    }", at));
}

/// `pub enum Scope`'s variants, in declaration order. The read starts at `Scope`'s own
/// declaration and stops at its closing brace, so `Ctx`'s variants above it and `NamedKey`'s
/// below it stay out; a variant is a line of its own at item indent, which no `///` line is.
function scopeVariants(source) {
  const from = source.indexOf("pub enum Scope {");
  assert.notEqual(from, -1, "keymap.rs still declares pub enum Scope");
  const body = source.slice(from, source.indexOf("\n}", from));
  return [...body.matchAll(/^ {4}(\w+),$/gm)].map(([, variant]) => variant);
}

/// The page's name for a scope: its variant's CamelCase words, lowercased and joined by a dot
/// (`OutputTree` → `output.tree`, `Global` → `global`).
function dotted(variant) {
  return variant.replace(/(?<=[a-z0-9])(?=[A-Z])/g, ".").toLowerCase();
}

/// Each `Scope` variant mapped to its dotted name.
function scopeNames(source) {
  return Object.fromEntries(scopeVariants(source).map((variant) => [variant, dotted(variant)]));
}

/// `NamedKey::from_token`'s arms, in order, as [variant, bare word], whether afkd writes an arm
/// `NamedKey::X` or `Self::X`.
function namedKeyWords(source) {
  return [...fnBody(source, "fn from_token(").matchAll(/"([a-z]+)" => (?:NamedKey|Self)::(\w+),/g)].map(
    ([, word, variant]) => [variant, word],
  );
}

/// `DEFAULT_KEYS`'s rows, in order, as `{scope, action, binding}` with each scope dotted.
function rustRows(source) {
  // The slice itself, bounded so a `(Scope::…, "…", "…")` tuple elsewhere in the file cannot
  // drift into the comparison.
  const from = source.indexOf("const DEFAULT_KEYS: &[(Scope, &str, &str)] = &[");
  assert.notEqual(from, -1, "keymap.rs still declares DEFAULT_KEYS as a slice of triples");
  const slice = source.slice(from, source.indexOf("\n];", from));
  const rows = [...slice.matchAll(/\(Scope::(\w+),\s*"([\w_]+)",\s*"([^"]*)"\)/g)].map(
    ([, scope, action, binding]) => ({ scope, action, binding }),
  );
  assert.ok(
    rows.length >= 51,
    `the DEFAULT_KEYS read found only ${rows.length} rows — the table's shape moved, so this test is comparing against a stub`,
  );
  // `Scope::OutputTree` → `output.tree`, named from the enum rather than transcribed, so a
  // scope renamed there fails here too.
  const names = scopeNames(source);
  assert.equal(Object.keys(names).length, 8, "Scope still declares eight variants");
  return rows.map(({ scope, action, binding }) => ({ scope: names[scope], action, binding }));
}

// --- AC1: the transcription is the Rust table ---------------------------------------

test("the 51 rows are DEFAULT_KEYS's, in order", { skip: NO_AFKD_SRC }, () => {
  assert.deepEqual(
    DEFAULT_KEYS,
    rustRows(KEYMAP_RS),
    "keymap.mjs's transcription is DEFAULT_KEYS row for row, scope for scope, binding for binding",
  );
  assert.equal(DEFAULT_KEYS.length, 51, "…and it is still 51 rows");
});

/// One edit of each kind a `DEFAULT_KEYS` row can take, as [what, needle, replacement]: a
/// binding changed, an action renamed, a row moved to another scope, a row dropped and a row
/// added.
const ROW_EDITS = [
  ["a binding changed", '(Scope::Global, "help", "?"),', '(Scope::Global, "help", "/"),'],
  ["an action renamed", '(Scope::Overview, "service_start", "s"),', '(Scope::Overview, "service_begin", "s"),'],
  ["a row moved to another scope", '(Scope::Info, "back", "i esc"),', '(Scope::Confirm, "back", "i esc"),'],
  ["a row dropped", '    (Scope::Confirm, "accept", "y"),\n', ""],
  [
    "a row added",
    '    (Scope::Global, "help", "?"),\n',
    '    (Scope::Global, "help", "?"),\n    (Scope::Global, "extra", "z"),\n',
  ],
];

test("a changed DEFAULT_KEYS row reddens the transcription", { skip: NO_AFKD_SRC }, () => {
  for (const [what, needle, replacement] of ROW_EDITS) {
    const edited = KEYMAP_RS.replace(needle, replacement);
    // A target the Rust no longer holds would make the edit a no-op and this check vacuous.
    assert.notEqual(edited, KEYMAP_RS, `${what}: keymap.rs still holds ${JSON.stringify(needle)}`);
    assert.throws(
      () => assert.deepEqual(DEFAULT_KEYS, rustRows(edited)),
      assert.AssertionError,
      `${what} in DEFAULT_KEYS fails the transcription`,
    );
  }
});

test("the scopes are Scope's, in its own order", { skip: NO_AFKD_SRC }, () => {
  const names = scopeVariants(KEYMAP_RS).map(dotted);
  assert.equal(names.length, 8, "the Scope read found the eight variants");
  assert.deepEqual(SCOPES, names, "SCOPES is Scope's variants in declaration order");
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
  // `named_glyph`'s arms, read out of the Rust and keyed by the word `NamedKey::from_token`
  // reads — so both halves of the pair come from the file rather than from this test.
  const token = Object.fromEntries(namedKeyWords(KEYMAP_RS));
  assert.equal(Object.keys(token).length, 11, "NamedKey::from_token still reads eleven words");
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
  // The deliberately partial actions, named rather than inferred: `o`/`i` open a **service**'s
  // run view and info page and have no subject on a group header or a lane row.
  assert.deepEqual(
    ACTIONS.filter((id) => HANDLED.has(id) && NOTES[id] !== undefined),
    PARTIAL,
    "only the declared PARTIAL actions are both handled and noted",
  );
  // Pinned in `ACTIONS` **table order**, which is what `keymap.mjs`'s own module-load check
  // compares against: an appended entry throws on import.
  assert.deepEqual(PARTIAL, ["overview.show_output", "overview.show_info"]);
});

// --- the lane keys ----------------------------------------------------------------------

/// The four lane actions, and afkd top's legend line for each (`help::help_lines`).
const LANE_LEGEND = {
  "overview.queue_widen": "Widen the service's queue lane",
  "overview.queue_narrow": "Narrow the service's queue lane",
  "queues.narrow": "Narrow the queue lane at the cursor",
  "queues.widen": "Widen the queue lane at the cursor",
};

test("the four lane keys are handled, carry no note and read afkd top's legend", () => {
  for (const [id, line] of Object.entries(LANE_LEGEND)) {
    assert.ok(ACTIONS.includes(id), `${id} is a row`);
    assert.ok(HANDLED.has(id), `${id} is handled: the relay has a lane verb`);
    assert.equal(NOTES[id], undefined, `${id} carries no note`);
    assert.equal(DESCRIPTIONS[id], line, `${id} reads afkd top's legend`);
  }
});

test("the lane legend is help.rs's own", { skip: NO_AFKD_SRC }, () => {
  // `keys.hint(act::LIST_QUEUE_WIDEN, "Widen the service's queue lane")`: each `act::` constant
  // is joined to its dotted id through `keys.rs` and `keymap.rs`, as the drain's test joins its.
  const scopes = scopeNames(KEYMAP_RS);
  const legend = {};
  for (const [, name, line] of HELP_RS.matchAll(/keys\.hint\(act::(\w+), "([^"]*)"\)/g)) {
    const decl = KEYS_RS.match(new RegExp(`const ${name}: Action = \\(Scope::(\\w+), "([\\w_]+)"\\);`));
    if (decl !== null) legend[`${scopes[decl[1]]}.${decl[2]}`] = line;
  }
  assert.ok(Object.keys(legend).length >= 10, `the help.rs read found only ${Object.keys(legend).length} hints`);
  for (const id of Object.keys(LANE_LEGEND)) {
    assert.equal(DESCRIPTIONS[id], legend[id], `${id} is described as help.rs lists it`);
  }
});

test("the drain refuses exactly the actions keys.rs refuses", { skip: NO_AFKD_SRC }, () => {
  // `keys::REFUSED_WHILE_QUITTING` is a list of `act::` constants; each constant's value is the
  // scope-qualified action name, so the two are joined through the `act` module's own
  // definitions rather than through a second transcription.
  // The read is bounded to the list itself — from its `= &[` to the closing `]` — and takes
  // each `act::` entry with or without a trailing comma, so `&[act::RELOAD]` and a multi-line
  // list read alike, whatever their length.
  const from = KEYS_RS.indexOf("pub(crate) const REFUSED_WHILE_QUITTING");
  assert.notEqual(from, -1, "keys.rs still declares REFUSED_WHILE_QUITTING");
  const open = KEYS_RS.indexOf("= &[", from);
  assert.notEqual(open, -1, "REFUSED_WHILE_QUITTING is still a `&[…]` list");
  const list = KEYS_RS.slice(open, KEYS_RS.indexOf("]", open));
  const names = [...list.matchAll(/act::(\w+)/g)].map(([, name]) => name);
  assert.ok(names.length > 0, "the refusal read found no act:: constants in REFUSED_WHILE_QUITTING");
  // `pub(crate) const LIST_START: Action = (Scope::Overview, "service_start");`
  const scopes = scopeNames(KEYMAP_RS);
  const rust = names.map((name) => {
    const decl = KEYS_RS.match(new RegExp(`const ${name}: Action = \\(Scope::(\\w+), "([\\w_]+)"\\);`));
    assert.notEqual(decl, null, `keys.rs declares act::${name}`);
    return `${scopes[decl[1]]}.${decl[2]}`;
  });
  assert.deepEqual(
    [...REFUSED_WHILE_QUITTING].sort(),
    [...rust].sort(),
    "the drain's refusal set is keys.rs's, so the footer and the dispatch refuse one set",
  );
  // …and every one of them is a real row, so a drain cannot refuse an action nothing binds.
  for (const id of REFUSED_WHILE_QUITTING) assert.ok(ACTIONS.includes(id), `${id} is a row`);
});

// --- the reads are bounded, and take either spelling of an arm ---------------------------

/// `keymap.rs`'s three enums and `NamedKey::from_token` as afkd writes them, each arm `Self::X`,
/// trimmed to what the reads cover: `Ctx`'s variants above `Scope` and `NamedKey`'s below it
/// share their shape, so the `Scope` read is bounded at both ends or it reads too many.
const KEYMAP_BY_SELF = `#[cfg(any(test, feature = "testutil"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Ctx {
    Global,
    Overview,
    /// The overview's queue-lane rows. Consulted before \`overview\`, which is live at the same
    /// instant, so it shadows it, as \`global\` shadows every view.
    Queues,
    OutputTree,
    OutputLog,
    Info,
    Confirm,
}

/// A keymap scope: a named view or an \`output\` sub-pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Process-global keys, live in every view (\`quit\`/\`reload\`/\`help\`).
    Global,
    /// The service overview list.
    Overview,
    /// The overview's queue-lane rows, live while the cursor sits on one; shadows
    /// [\`Overview\`](Scope::Overview).
    Queues,
    /// The output view's frame (focus, scope, back), active whichever pane is shown.
    Output,
    /// The output view's tree pane (active while the tree has focus).
    OutputTree,
    /// The output view's log pane (active while the log has focus).
    OutputLog,
    /// The service info view.
    Info,
    /// The confirm modal (accept/cancel).
    Confirm,
}

/// A named key in the binding vocabulary (a bare word like \`enter\` / \`pageup\`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NamedKey {
    /// The up-arrow key.
    Up,
    /// The down-arrow key.
    Down,
    /// The left-arrow key.
    Left,
    /// The right-arrow key.
    Right,
    /// The enter/return key.
    Enter,
    /// The space bar (as a named key, distinct from a literal space glyph).
    Space,
    /// The escape key.
    Esc,
    /// The tab key.
    Tab,
    /// The page-up key.
    PageUp,
    /// The page-down key.
    PageDown,
    /// The backspace key.
    Backspace,
}

impl NamedKey {
    /// Parses a bare word into a named key, or \`None\` when the word is not one.
    fn from_token(s: &str) -> Option<Self> {
        Some(match s {
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "enter" => Self::Enter,
            "space" => Self::Space,
            "esc" => Self::Esc,
            "tab" => Self::Tab,
            "pageup" => Self::PageUp,
            "pagedown" => Self::PageDown,
            "backspace" => Self::Backspace,
            _ => return None,
        })
    }

    /// A second method's arms, which the \`from_token\` read must not take.
    fn other(s: &str) -> Option<Self> {
        Some(match s {
            "home" => Self::Up,
            _ => return None,
        })
    }
}
`;

/// The same text with each arm spelled by its type, `NamedKey::X`, as afkd wrote arms before
/// clippy's `use_self`.
const KEYMAP_BY_TYPE = KEYMAP_BY_SELF.replaceAll("=> Self::", "=> NamedKey::");

test("the Scope and from_token reads take an arm spelled NamedKey:: or Self::", () => {
  assert.notEqual(KEYMAP_BY_TYPE, KEYMAP_BY_SELF, "the fixture's arms are respelled");
  for (const source of [KEYMAP_BY_TYPE, KEYMAP_BY_SELF]) {
    // Eight, not `Ctx`'s seven before them or `NamedKey`'s eleven after: the read is bounded.
    assert.deepEqual(scopeVariants(source), [
      "Global",
      "Overview",
      "Queues",
      "Output",
      "OutputTree",
      "OutputLog",
      "Info",
      "Confirm",
    ]);
    assert.deepEqual(scopeVariants(source).map(dotted), SCOPES, "…the list the live test holds SCOPES to");
    // Eleven, not the twelve arms the two methods hold between them.
    const token = Object.fromEntries(namedKeyWords(source));
    assert.equal(Object.keys(token).length, 11);
    assert.equal(token.PageDown, "pagedown");
  }
  // A real change still shows: a scope dropped reads seven and a word dropped reads ten, which
  // the live counts red on.
  const confirmDropped = KEYMAP_BY_SELF.replace("    Confirm,\n}\n\n/// A named key", "}\n\n/// A named key");
  assert.equal(scopeVariants(confirmDropped).length, 7);
  assert.equal(namedKeyWords(KEYMAP_BY_SELF.replace('            "tab" => Self::Tab,\n', "")).length, 10);
  assert.throws(
    () => namedKeyWords(KEYMAP_BY_SELF.replace("fn from_token(", "fn parse(")),
    /keymap\.rs still has fn from_token\(/,
  );
  assert.throws(
    () => scopeVariants(KEYMAP_BY_SELF.replace("pub enum Scope {", "pub enum Pane {")),
    /still declares pub enum Scope/,
  );
});
