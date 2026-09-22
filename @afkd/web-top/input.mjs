// The key seam: a browser `keydown` turned into an afkd chord, dispatched through the session,
// and the resulting verbs posted to the relay.
//
// It is its own module rather than three functions in `top.mjs` for one reason: `top.mjs` runs
// its whole boot at import — `getElementById`, `new EventSource`, a `ResizeObserver`, two
// timers — so a suite that imported it would have to fake a browser large enough to be wrong.
// Everything here is pure given an element and the handful of effects it is handed, so
// `input.test.mjs` holds it against a stub with no browser underneath. That is
// `paint.test.mjs`'s bargain exactly.
//
// **Keys are captured, not stolen.** The page takes a key only while the grid has focus, and
// the chords a browser reserves are an explicit table below rather than a fallthrough: afkd's
// `Ctrl+R` is intercepted, and `Ctrl+T`/`Ctrl+W`/`Ctrl+L`/`Ctrl+N`/`Ctrl+C`/`F5` are left to
// the browser untouched.

import { flash, press, scrollInfo } from "./session.mjs";

/// `event.key`'s spelling of each named key the binding table uses, mapped to the table's own
/// token. The browser twin of `shell::chord_of`, which does the same job from crossterm's
/// `KeyCode`. A key outside this map and outside the single-char case is inert, which is
/// `chord_of`'s own `None`.
const NAMED_TOKEN = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  Enter: "enter",
  " ": "space",
  Escape: "esc",
  Tab: "tab",
  PageUp: "pageup",
  PageDown: "pagedown",
  Backspace: "backspace",
};

/**
 * The afkd chord a `keydown` names — `{ctrl, key}` with `key` a named token or a single
 * character — or `null` for a key with no binding vocabulary.
 *
 * A printable character passes through with its **case**, which is semantic: `g` and `G` are
 * two keys, and the browser already delivers the shifted glyph. `Alt` and `Meta` chords are
 * inert: the binding grammar has no spelling for them, so a page that swallowed one would be
 * eating a chord it could never resolve.
 */
export function chordOf(event) {
  if (event.altKey === true || event.metaKey === true) return null;
  const named = NAMED_TOKEN[event.key];
  if (named !== undefined) return { ctrl: event.ctrlKey === true, key: named };
  // A dead key, a lone modifier and every F-key arrive as a multi-character name; none has a
  // binding, so each is left alone.
  if (typeof event.key !== "string" || [...event.key].length !== 1) return null;
  return { ctrl: event.ctrlKey === true, key: event.key };
}

/**
 * The chords a browser has its own meaning for, and what this page does with each.
 *
 * `taken` is afkd's and is **always** `preventDefault`ed — even while the daemon is draining
 * and the dispatch refuses it, because a Ctrl+R that sometimes reloads the tab is worse than
 * one that never does. `left` is the browser's and is never dispatched and never prevented:
 * a new tab, a closed tab, the address bar, a new window, the clipboard and a hard refresh all
 * keep working while the grid has focus. `global.quit` is unhandled here anyway, so `Ctrl+C`
 * costs nothing to leave.
 *
 * Matched on the **event**, not on the chord, so `F5` — which `chordOf` has no token for — is
 * in the table by name rather than by the accident of resolving to nothing.
 */
export const RESERVED = {
  taken: [{ ctrl: true, key: "r" }],
  left: [
    { ctrl: true, key: "t" },
    { ctrl: true, key: "w" },
    { ctrl: true, key: "l" },
    { ctrl: true, key: "n" },
    { ctrl: true, key: "c" },
    { ctrl: false, key: "F5" },
  ],
};

/// Whether `event` is one of `table`'s chords.
function reserved(table, event) {
  return table.some((c) => c.ctrl === (event.ctrlKey === true) && c.key === event.key);
}

/// Whether the grid owns the keyboard right now. Read off the element's **own** document, so
/// the gate lives here rather than in the caller's predicate: a key struck while the operator
/// is in the address bar, or in any other focusable thing on the page, is not this page's.
function focused(element) {
  return element.ownerDocument?.activeElement === element;
}

/// One posted command body — `{stream, command, service}`, all three fields, because the relay
/// keys a command to **this** subscriber's own attach and 404s a body whose `stream` names
/// none. A global verb carries no service: the relay composes a `Reload` frame from the verb
/// alone, and a `service` key it did not ask for would be a field smuggled onto the wire.
function bodyOf(streamId, command) {
  return command.service === undefined
    ? { stream: streamId, command: command.command }
    : { stream: streamId, command: command.command, service: command.service };
}

/**
 * Wire the grid's keyboard up. Every effect arrives as a dependency, so this module names no
 * global and the suite hands it a stub:
 *
 * - `element` — the grid. It takes focus on load and on `pointerdown`, and the `keydown`
 *   listener sits on it, so nothing is captured while the operator is elsewhere.
 * - `read()` → `{session, board, streamId, bodyHeight, infoMax, run}`, the state a press is
 *   resolved against.
 * - `write(session)` — store the next session.
 * - `post(body)` → `Promise<{ok, message}>`; the `POST /command` call.
 * - `now()` — the clock the flash is stamped against.
 * - `repaint()` — draw, rather than waiting on the throttle a key press should outrun.
 */
export function installKeys({ element, read, write, post, now, repaint }) {
  element.focus();
  element.addEventListener("pointerdown", () => element.focus());
  element.addEventListener("keydown", (event) => {
    if (!focused(element)) return;
    if (reserved(RESERVED.left, event)) return;
    const take = reserved(RESERVED.taken, event);
    const chord = chordOf(event);
    if (chord === null) {
      if (take) event.preventDefault();
      return;
    }
    const { session, board, streamId, bodyHeight, run } = read();
    const out = press(session, board, chord, { now: now(), bodyHeight, run });
    write(out.session);
    if (out.handled || take) event.preventDefault();
    for (const command of out.commands) send(command, streamId, { read, write, post, now, repaint });
    repaint();
  });
  // The info page's scroll gesture. It is the **wheel** rather than a key because the `info`
  // scope binds no nav action and inventing one would make `keymap.mjs`'s transcription of
  // afkd's table a lie; a wheel is a browser affordance, the same register as the `RESERVED`
  // table and `pointerdown` focus, and it costs no keymap row. Gated on the page being open, so
  // a wheel over the list is still the browser's.
  element.addEventListener("wheel", (event) => {
    if (!focused(element)) return;
    const { session, infoMax } = read();
    if (session.info === null) return;
    const next = scrollInfo(session, wheelRows(event), infoMax ?? 0);
    // Only a wheel that actually moved the page is taken: at either end the gesture is the
    // browser's again, so a page already at its floor does not silently eat the flick.
    if (next === session) return;
    write(next);
    event.preventDefault();
    repaint();
  });
}

/// One wheel event in **rows**, honouring `WheelEvent.deltaMode`: `0` is pixels (a line is the
/// browser's ~16, and one screen row is one line here), `1` is already lines, `2` is pages.
///
/// The magnitude is rounded and the sign re-applied, rather than rounding the signed number:
/// `Math.round` breaks a half toward `+∞`, so a notch worth 7.5 rows would scroll eight down and
/// seven up and the page would creep. Rounded **away from zero** at the floor as well, so the
/// smallest trackpad flick still moves a row rather than nothing.
const WHEEL_PIXELS_PER_ROW = 16;
const WHEEL_ROWS_PER_PAGE = 10;
function wheelRows(event) {
  const delta = event.deltaY ?? 0;
  if (delta === 0) return 0;
  const rows =
    event.deltaMode === 1
      ? delta
      : event.deltaMode === 2
        ? delta * WHEEL_ROWS_PER_PAGE
        : delta / WHEEL_PIXELS_PER_ROW;
  return Math.sign(rows) * Math.max(1, Math.round(Math.abs(rows)));
}

/// Post one command and, if the relay refused it, say so where the ack was.
///
/// The ack the press wrote is already in the session — `dispatch_commands` computes it before
/// the writes for the same reason — so a refusal **overwrites** it rather than appending to it:
/// a command that failed must not leave `Fired web` on screen. The session is re-read at reply
/// time because frames have landed in between, and the board is not this function's to freeze.
function send(command, streamId, { read, write, post, now, repaint }) {
  post(bodyOf(streamId, command)).then((reply) => {
    if (reply.ok) return;
    write(flash(read().session, reply.message, now()));
    repaint();
  });
}
