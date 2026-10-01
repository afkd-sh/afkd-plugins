# `@afkd/trello`

A **provider** plugin with one trigger kind, [`service(trello)`](#servicetrello-cards): it
fires when a card lands in a watched list of a Trello board, and gives the service's slots
the actions that move the card across the board as work starts, finishes, or fails. It also
ships the `trello` skill an agent works the card through: read and post comments, fetch and
attach files, tick checklist items, move and label the card, and ask a question that parks
the card until a human replies.

It is afkd's built-in Trello trigger, moved out of afkd: the same keys, the same claim,
watermark, attempt and park comments on the card, the same claim-journal keys and session
threads, the same run environment and the same brief. A claim or a park the built-in left
on a live card is recognised, renewed and released by the plugin, and the other way round,
so switching from one to the other strands nothing. The few places the plugin behaves
differently are listed [at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[config language v2](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, built from source when it is installed.

## Install it

```console
$ afkd install @afkd/trello
```

From a checkout of this repository, name the directory instead:

```console
$ afkd install /path/to/afkd-plugins/@afkd/trello
```

afkd copies the tree and runs `cargo build --release --locked` in it, so the host needs a
Rust toolchain — the one afkd itself was installed with is enough. The first build fetches
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs).

Run it on **afkd 0.2.197 or newer**, the first with handle types and slot signatures: this
is a manifest v2 plugin, which a [config language v2](https://afkd.sh/docs/lang-v2/) file
imports; afkd passes its slots the claimed card as a `trello.Card`, and sends each action a
slot calls on a card as a `call`. An older afkd refuses the manifest when it is installed.

A config file uses the plugin by importing it, and then names it by its leaf, `trello`:

```conf
import "@afkd/trello"

TRELLO_BOARD   :: "https://trello.com/b/REPLACE_ME/board"
TRELLO_API_KEY :: env.TRELLO_API_KEY
TRELLO_TOKEN   :: env.TRELLO_TOKEN

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task.md
}

develop :: service(trello) {
  board         TRELLO_BOARD
  api_key       TRELLO_API_KEY
  token         TRELLO_TOKEN
  pick_from     "Up for Grabs"
  min_age       1m
  max_attempts  2
  poll_interval 1m to 3m

  on_claim(run: afkd.Run, card: trello.Card) {
    trello.add_member(card, trello.me)
    trello.move_to(card, "In Progress", at=top)
  }
  on_done(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.move_to(card, "Review", at=top)
    trello.comment(card, "afkd landed this card in #{outcome.duration}.")
  }
  on_park(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.comment(card, "parked after #{outcome.duration}: waiting for a reply.")
  }
  on_fail(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.move_to(card, "Backlog", at=bottom)
    trello.add_label(card, "Problem")
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) { task() }
}
```

The credentials come from the daemon's environment, so they never sit in the file. Each
action names the card it acts on — the `card` its slot is passed — so it reads the same
wherever it is written, and a slot may act on another card than the one its run claimed.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
// The agent a `service(trello)` runs: the skill is what lets it work the card.
builder :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/trello/trello" ]
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`TRELLO_API_KEY`, `TRELLO_TOKEN`, `TRELLO_BOARD_ID` and `TRELLO_CARD_ID` every
`service(trello)` run carries.

## `service(trello)` (cards)

Fires when a card lands in a watched list; its slots move cards across the board as work
starts, finishes, or fails. Its secrets stay with the plugin and the run.

| Key               | Type           | Notes                                  |
|-------------------|----------------|----------------------------------------|
| `board`           | `string`       | board address; required                |
| `api_key`         | `string`       | Trello API key; required               |
| `token`           | `string`       | Trello token; required                 |
| `base_url`        | `string`       | API base; defaults to the public Trello API (a test/overseer seam) |
| `pick_from`       | `string`       | list new cards are drawn from          |
| `require_member`  | `string`       | `"me"` or a username: only claim cards this member is already on; default unset (the head card is claimed) |
| `require_label`   | `string`       | only claim cards carrying the named label; default unset (no label filter) |
| `without_label`   | `list[string]` | never claim cards carrying any of these labels (deny mirror of `require_label`, deny wins); default unset |
| `discuss_with`    | `list[string]` | `[ "anyone" ]` or members, `[ "alice", "me" ]`: claim a card on first sight (afkd never commented) or when an allowed author comments after afkd last did; default unset |
| `min_age`         | `duration`     | don't claim a card younger than this, measured from card creation; default `0` (no filter) |
| `follow_comments` | `duration`     | re-read the card this often **while its run is in flight**, delivering new comments to the working agent; a range `30s to 90s` jitters; default unset (no mid-run watch) |
| `max_attempts`    | `int`          | retries per card; default `1`          |
| `poll_interval`   | `duration`     | default `30s`; a range `2m to 3m` jitters |

The last three are afkd's own, read by afkd for every kind that claims its work.

The service's slots are code ([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/)), each
passed the run, the claimed card, and after the run its outcome — by position, and a slot
writes every one of them, in order, each with its type:

| Slot                                                                | Runs                                            |
|---------------------------------------------------------------------|-------------------------------------------------|
| `on_run(run: afkd.Run, card: trello.Card)`                          | the run itself                                  |
| `on_claim(run: afkd.Run, card: trello.Card)`                        | once the card is claimed, before the run        |
| `on_done(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)`  | after a run that finished                       |
| `on_park(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)`  | after a run that ended waiting on a human reply |
| `on_fail(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)`  | after a run that failed                         |

`run` is an `afkd.Run` (`id`, `scratch_dir`) and `outcome` an `afkd.Outcome` (`ok`,
`duration`, `error`). `card` is a **`trello.Card`**, the plugin's handle for the card:

| Field    | Type           | What it is                                         |
|----------|----------------|----------------------------------------------------|
| `id`     | `string`       | the card's short link, `Qb7eLy2w`                  |
| `key`    | `string`       | afkd's claim key for the card                      |
| `title`  | `string`       | the card's name                                    |
| `url`    | `string`       | the card's address on trello.com                   |
| `labels` | `list[string]` | the names of the labels on the card                |

A config passes a card around and compares it, but never builds one. afkd carries `id` and
`key` at run time today; reading `title`, `url` or `labels` type-checks, and fails the slot
when it runs until afkd carries them too.

afkd runs a slot's statements in the order they are written, and each action it calls is
one request to the plugin, which does it on the card the action is passed first:

| Action                                   | Effect                                  |
|------------------------------------------|-----------------------------------------|
| `trello.add_member(card, <member>)`      | add a member to the card; additive and idempotent |
| `trello.remove_member(card, <member>)`   | remove a member from the card; idempotent (removing a member not on the card is a no-op) |
| `trello.mark_complete(card)`             | mark the card complete                  |
| `trello.move_to(card, "<list>", at=top)` | move the card to a list, at `top` (the default) or `bottom` |
| `trello.remove_label(card, "<name>")`    | remove a named label from the card; idempotent (removing a label not on the card is a no-op) |
| `trello.add_label(card, "<name>")`       | add a named label to the card           |
| `trello.comment(card, "<text>")`         | post a comment on the card, verbatim; in a post-run slot, `#{outcome.duration}` interpolates how long the run took |
| `trello.archive(card)`                   | archive the card                        |

The card is a `trello.Card` and nothing else: `trello.move_to("Review")`, with no card, or
passed another plugin's handle, is refused when the config loads.

A `<member>` is a username, or **`trello.me`**, the member the plugin's credentials
authenticate as. `trello.me` is Trello's own alias for that member, the string `"me"`, so a
setting, which afkd reads before the plugin runs, spells it `"me"`; it cannot name a real
member, since a Trello username is at least three characters long.

Each action returns a result, like any call: one the board refused — a list the board does
not have, a member not on it — fails with the plugin's sentence. A failing `on_claim` gives
the card back and fails the run; a failing post-run slot is logged and changes nothing.
afkd never retries a slot's action; the plugin retries what Trello could not take.

**When Trello cannot be reached.** An action Trello could not be reached for, or answered
with a 429 or a 5xx, is queued and the call answers ok, so the slot's next action still runs
and is queued behind it. The plugin retries the queue in the background, each card's writes
in the order they were asked for: the first retry 5 seconds after the failure, the gap
doubling to 5 minutes, for up to an hour. The `[afkd-attempt]` notes and the run-end
`[afkd-ran]` watermark are retried the same way. Every write queued is one line in the
service log; one given up is a loud line naming the card and what was lost. The queue is
held in memory, so a plugin afkd ends names each write it still owes.

**Repeating an action.** The label and member actions are all idempotent, so a retried card
cannot fault on its own slots: `add_label` and `add_member` are additive — attaching
what is already attached is a no-op — and `remove_label` and `remove_member` succeed
against a card that never carried the label or the member. The asymmetry to know is
board-level: `add_label` **creates** the named label on the board when it does not exist
yet, while `remove_label` never does (a name the board does not know simply has nothing to
remove); a *member* who is not on the board is an error either way. `move_to` is the one
action whose effect is positional rather than additive — it re-lists the card at the
target's `top` or `bottom` — so two `move_to`s leave the card wherever the last one put it.

**Cadence.** **Polls** the board every `poll_interval` and fires for cards in `pick_from`;
a service works **one card at a time, to completion**, and `max_attempts` bounds the
retries per card. The slots move, complete, or archive the card as work starts, finishes,
or fails.

**Intake.** By default the poll claims the card at the head of `pick_from`. With
`require_member` set, it claims the **first eligible** card instead — the first one the
named member is already on — so an unassigned card at the top of the list does not block
the ones under it. `require_label` is the label mirror: it claims only cards carrying a
label of that exact name, and composes with `require_member` (both must hold), so a
grooming service and an implementer can share one list — the implementer building only the
labelled cards. `without_label` is the negative gate — a card carrying any of the named
labels is left in place (a lighter opt-out than unassigning or moving lists), and it
accepts one or many labels; **deny wins** over `require_label`, so a card carrying both a
required and an excluded label is skipped. `min_age` is the one gate about **time** rather
than card content: a card younger than it is left alone — long enough for a human to finish
typing one — and, like the others, it *skips* rather than blocks, so a brand-new card at the
top does not hold up the settled ones under it. Age is measured from the card's
**creation** and nothing else: a comment or an edit on an old card does not reset it, and
`min_age` never delays a `discuss_with` tail. The gate is evaluated at claim time only, and
it is not the claim lock.

```conf
import "@afkd/trello"

implement :: service(trello) {
  board          "https://trello.com/b/REPLACE_ME/board"
  api_key        "REPLACE_ME"
  token          "REPLACE_ME"
  pick_from      "To Do"
  require_member "me"
  require_label  "ready"
  without_label  [ "blocked", "needs design — ask 陳大文" ]
  min_age        10m

  on_claim(run: afkd.Run, card: trello.Card) {
    trello.move_to(card, "In Progress", at=top)
    trello.remove_label(card, "ready")
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Grooming.** With `discuss_with` set, a card is claimed on **first sight** — afkd has never
commented on it, so a freshly-assigned card with a description and **no comments** is
groomed on the next poll (assignment is the opt-in) — or, once afkd has spoken, when its
**tail** (the comments posted after afkd's own last comment on that card) carries a comment
from an allowed author. `discuss_with [ "anyone" ]` allows any author but afkd itself;
`discuss_with [ "alice", "bob" ]` allows only those named members (resolved the same
`"me"`-or-username way as `require_member`, with afkd's own member always excluded, and a
list naming `"anyone"` beside a member reading it as a username); on first sight the
allow-list does not apply — both fire the moment the card is seen. This turns a card into a
comment-driven back-and-forth in place, with no list moves: it **fires on first sight**,
then **goes quiet** after afkd comments, and **re-fires** only when an allowed author
comments again after afkd's last word — several human comments in one tail collapse into a
single turn. The allow-list only gates *re*-firing after afkd's first comment. The tail
boundary is decided by comment **author**, not marker text, so a comment posted as afkd
never re-fires the card — with one exception, afkd's own **claim marker**, which is the
lease rather than speech: one left behind by a run whose release never reached the board
would otherwise become the boundary and silently stop the card being picked at all.
Omitting the key leaves the claim path unchanged; `anyone` is an explicit value, not the
same as omitting it. Every turn ends with afkd as the last speaker: if the agent posts
nothing, the plugin posts one terse backstop comment so the card does not re-fire on the
next poll.

```conf
import "@afkd/trello"

groom :: service(trello) {
  board        "https://trello.com/b/REPLACE_ME/board"
  api_key      "REPLACE_ME"
  token        "REPLACE_ME"
  pick_from    "Ideas"
  discuss_with [ "alice", "bob" ]

  on_claim(run: afkd.Run, card: trello.Card) { trello.add_member(card, trello.me) }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**The clarification gate.** A run on a card has a third outcome between finish and fail:
when the card is too ambiguous to build — or its premise does not hold at all — the agent
can **ask first**. Give an agent the `@afkd/trello/trello` skill (see
[above](#name-the-skill)); its **ask** action posts the agent's
message on the card and writes the marker `park` into the run's scratch dir
(`$AFKD_SCRATCH_DIR`). The plugin reads that marker at the end of the run and *parks* the
card instead of failing it: it adds the `Awaiting Reply` label, releases the claim, and
**moves nothing** — the card keeps its place, its list, and its unspent attempts. `on_fail`
does not run, no `[afkd-attempt]` marker goes up, and the poll goes on to the next card. The
label is managed by the plugin itself, so the gate holds with no `on_park` slot; `on_park`
is only for extras (`on_park(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Discussion") }` for a board
that wants parked cards gathered somewhere).

The badge is also how afkd **finds the card again**. Once a beat, before it reads
`pick_from`, the plugin asks the board which of its open cards carry `Awaiting Reply`; a
badged card whose thread has gained a comment afkd did not write, newer than afkd's own last
word, is claimed again right there — wherever it sits on the board, ahead of anything queued
— the badge comes off, and the reply arrives under **New comments** in the fresh brief. A
badged card nobody has answered is left alone. Badge it with a `without_label` name to veto
it; archive it and it stops being scanned at all. An idle beat costs exactly one extra board
request.

A parked card goes back to the **service that parked it** — any instance of it, so a pool
copy resumes what its base asked, and no other service's sweep takes it. A card parked by a
service you have since renamed or removed is nobody's, so it is picked up by whoever can
take it, with one line in the log naming the service that let go of it. While the service
that parked a card is still configured, no other service takes that card, **in any list** —
dragging it into another service's `pick_from` changes nothing. Answer the question, or take
the badge off by hand.

**Stop the run at the ask, yourself.** The marker is read whatever signal the run ended
with, but the engine does not halt the workflow for you. A single-step workflow needs no
gate — the run ends at the ask. A **multi-step** one must gate the rest on the marker, so
nothing runs past the question:

```conf
import "@afkd/trello"

// The agent the service below calls. Minimal, so this snippet validates on its own.
builder :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/trello/trello" ]
}

widgets :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "To Do"

  on_claim(run: afkd.Run, card: trello.Card) { trello.move_to(card, "In Progress", at=top) }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) {
    builder <- "build the card; ask through the trello skill if unclear"
    if fs.is_file("#{run.scratch_dir}/park") { fail "parked: awaiting a human reply" }
    // reviewer / commit steps below never run when the agent asked
  }
}
```

The `fail` aborts the run; the plugin reclassifies that fault into the park. The scratch dir
is **outside** any `with git.worktree(…)` copy, so the marker survives the copy being torn
down and agent, gate, and plugin all name the same file.

**The badge is a lock**, held by the service that parked the card — it is the findability
too, but only its holder takes it off. A service whose `on_claim` does not move the card out
of `pick_from` will re-claim **its own** parked card on the next beat, exactly as an
`on_done`-less config re-fires — drag it elsewhere, or give the service an
`on_claim(run: afkd.Run, card: trello.Card) { trello.move_to(card, …) }`, as the example below does. A human who
drags a badged card back into the parking service's own `pick_from` gets it claimed and
unbadged there, answered or not.

```conf
import "@afkd/trello"

widgets :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "To Do"

  on_claim(run: afkd.Run, card: trello.Card) { trello.move_to(card, "In Progress", at=top) }
  on_done(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Review", at=top) }
  on_fail(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Backlog", at=bottom) }
  on_park(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.comment(card, "parked after #{outcome.duration} — waiting on you")
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Talking to a run in progress.** Everything above happens *between* runs: the brief is a
snapshot taken at claim time, so a comment posted while the agent is working is invisible to
the run it is about — and on a board whose `on_done` archives the card, it is invisible for
good. `follow_comments <duration>` closes that window. With it set, afkd asks the plugin for
the claimed card's comments on that interval for the extent of the run and hands any new one
to the agent **that is already working**, as another turn in the same conversation:

```conf
import "@afkd/trello"

widgets :: service(trello) {
  board           "https://trello.com/b/REPLACE_ME/board"
  api_key         "REPLACE_ME"
  token           "REPLACE_ME"
  pick_from       "To Do"
  poll_interval   2m to 3m
  follow_comments 60s          // the agent hears you mid-run

  on_claim(run: afkd.Run, card: trello.Card) { trello.move_to(card, "In Progress", at=top) }
  on_done(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.mark_complete(card)
    trello.move_to(card, "Done", at=bottom)
    trello.archive(card)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

Every delivered message is **also appended to the run's `task.md`**, so the next agent of a
multi-agent run reads the correction in the task itself, and a retry keeps it. Comments are
delivered once each, oldest-first, attributed exactly as a brief's `## New comments` section
is; afkd's own comments and claim markers are never delivered back, and the comments the
brief was built from are never delivered again. Cost: one extra comment read per interval
**per running card**, plus one at the end of each run, and nothing at all while no run is in
flight. All of this is afkd's own mid-run watch; the plugin only reads the thread for it.

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **afkd runs the slots.** The built-in ran its lifecycle blocks itself, in a fixed order
  and inside its own claim and finish. Here a slot is code afkd runs in the order it is
  written, and each action is one request to the plugin naming the card it acts on, sent
  once the plugin's own claim or finish has landed: `on_claim` after the claim is won, and
  the post-run slot after the claim is released — so a card moved by `on_done` is, for a
  moment, unclaimed in its old list. afkd retries no slot's action.
- **`me`, not `self`.** The member the credentials authenticate as is `trello.me` in a
  slot and `"me"` in a setting — Trello's own alias for it — where the built-in reserved
  the word `self`.
- **Some settings are refused when the service starts, not at `afkd validate`.** afkd types
  the settings against the plugin's manifest before the plugin ever runs. What it cannot
  see — a list that names nothing, a `min_age` that is a range — the plugin refuses when it
  is greeted, in the built-in's own words, and the service ends there.
- **An undelivered finish is held for afkd to release.** When the plugin cannot finish a
  card — the park's badge or owner marker, or the claim's release, does not land — the
  built-in keeps the claim and its reaper finishes it on a later beat. The plugin answers
  afkd's `finish` with `held`, and afkd does the same: it keeps the claim and asks the
  plugin to release it on a later beat, which replays what is owed.
- **No stop mid-claim.** The built-in abandons a claim a shutdown lands in the middle of;
  the plugin cannot see afkd stop, so it finishes the claim, and afkd hands the unit
  straight back with a release. The card ends where the built-in leaves it.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole card through the skill. A thread with more
  new comments than fit in one reply delivers the newest, and names the ones left out in the
  service log.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, and each claim attempt waits a second for a rival's marker to show, so a scan that
  keeps losing races stops after 20 and leaves the rest of the list for the next poll. And
  every call the plugin answers comes back within 45 seconds: a board too slow to answer
  in that time is treated as if it were down.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  card Qb7eLy2w` where the built-in's opens `# Work item from trello card Qb7eLy2w`, and a
  comment delivered mid-run calls the card "this work item" rather than "this card".
