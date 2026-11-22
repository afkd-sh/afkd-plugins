# `@afkd/trello`

A **provider** plugin with one trigger kind, [`service(trello)`](#servicetrello-cards): it
fires when a card lands in a watched list of a Trello board, and gives the service's slots
the actions that move the card across the board as work starts, finishes, or fails, and
that attach a file to the card and fetch one back. Beside it, a config
[reads Trello through a `trello.Account`](#reading-trello-through-a-trelloaccount): a board's
cards, a card's comments, checklists and attachments, and a board's lists, labels and members,
from any service's slots or run. It also
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
[the afk language](https://afkd.sh/docs/reference/) for the configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/trello
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest, the `afkd-trello` binary, its `skills/` and the `LICENSE-MIT` and
`LICENSE-APACHE` texts, nothing else. Nothing is compiled on your host, and no Rust
toolchain is needed there. Installing by name needs **afkd 0.2.714 or newer**, the first
whose unit brief is the folder `task/`, where this plugin's `task/README.md` is framed;
with it, `afkd update @afkd/trello` also moves a copy an older afkd built from source onto
the finished release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/trello/Cargo.toml
$ tools/pack-release trello /tmp/afkd-release @afkd/trello/target/release/afkd-trello
$ afkd install /tmp/afkd-release/afkd-trello
```

Run it on **afkd 0.2.241 or newer**, the first that hands a slot the fields this plugin
sends with a claim: this is a manifest v2 plugin, which a
[config in the afk language](https://afkd.sh/docs/reference/) imports; afkd passes its slots the
claimed card as a `trello.Card`, and sends each action a slot calls on a card as a `call`.
afkd 0.2.197 to 0.2.240 install and run it too, but there a slot reading any field beyond
`id` and `key` fails; an older afkd refuses the manifest when it is installed.

The configs below are written in the afk language of **afkd 0.2.704 or newer**, the
first that writes a top-level agent as a block, `agent(claude) { … }`; an older afkd still
runs the plugin, but refuses the examples when they load.

A config file uses the plugin by importing it, and then names it by its leaf, `trello`:

```conf
import "afkd"
import "std:env"
import "@afkd/trello"

TRELLO_BOARD   :: "https://trello.com/b/REPLACE_ME/board"
TRELLO_API_KEY :: env.get("TRELLO_API_KEY") ?? fail("TRELLO_API_KEY is unset")
TRELLO_TOKEN   :: env.get("TRELLO_TOKEN") ?? fail("TRELLO_TOKEN is unset")

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task/README.md
}

develop :: service(trello) {
  board        TRELLO_BOARD
  api_key      TRELLO_API_KEY
  token        TRELLO_TOKEN
  pick_from    "Up for Grabs"
  min_age      1m
  max_attempts 2
  every        1m~3m

  on_claim proc(card: trello.Card) {
    trello.add_member(card)
    trello.move_to(card, "In Progress", at=.top)
  }
  on_done proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.move_to(card, "Review", at=.top)
    trello.comment(card, "afkd landed this card in #{outcome.duration}.")
  }
  on_park proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.comment(card, "parked after #{outcome.duration}: waiting for a reply.")
  }
  on_fail proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.move_to(card, "Backlog", at=.bottom)
    trello.add_label(card, "Problem")
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) { task() }
}
```

The credentials come from the daemon's environment, so they never sit in the file. Each
action names the card it acts on — the `card` its slot is passed — so it reads the same
wherever it is written, and a slot may act on another card than the one its run claimed.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
import "ai:claude"

// The agent a `service(trello)` runs: the skill is what lets it work the card.
builder :: agent(claude) {
  model  "sonnet"
  skills "@afkd/trello/trello"
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`TRELLO_API_KEY`, `TRELLO_TOKEN`, `TRELLO_BOARD_ID` and `TRELLO_CARD_ID` every
`service(trello)` run carries.

## `service(trello)` (cards)

Fires when a card lands in a watched list; its slots move cards across the board as work
starts, finishes, or fails. Its secrets stay with the plugin and the run.

| Key              | Type           | Notes                                  |
|------------------|----------------|----------------------------------------|
| `board`          | `str`          | board address; required                |
| `api_key`        | `str`          | Trello API key; required               |
| `token`          | `str`          | Trello token; required                 |
| `base_url`       | `str`          | API base; defaults to the public Trello API (a test/overseer seam) |
| `pick_from`      | `list[str]`    | the lists new cards are drawn from, in the order written; one name or several; required |
| `require_member` | `str`          | `"me"` or a username: only claim cards this member is already on; default unset (the head card is claimed) |
| `require_label`  | `str`          | only claim cards carrying the named label; default unset (no label filter) |
| `without_label`  | `list[str]`    | never claim cards carrying any of these labels (deny mirror of `require_label`, deny wins); default unset |
| `discuss_with`   | `list[str]`    | `[ "anyone" ]` or members, `[ "alice", "me" ]`: claim a card on first sight (afkd never commented) or when an allowed author comments after afkd last did; default unset |
| `min_age`        | `duration`     | don't claim a card younger than this, measured from card creation; default `0` (no filter) |
| `max_attempts`   | `int`          | retries per card; default `1`          |
| `every`          | `duration`     | the gap between two checks for work; default `30s`; a range `2m~3m` jitters |

The last two are afkd's own, read by afkd for every kind that claims its work.

The service's slots are code ([reference §10.3](https://afkd.sh/docs/reference/#103-slots)), each
passed the run (all but `on_claim`, which runs before one exists), the claimed card, and
after the run its outcome — by position, and a slot writes every one of them, in order,
each with its type:

| Slot                                                                    | Runs                                            |
|-------------------------------------------------------------------------|-------------------------------------------------|
| `on_run proc(run: afkd.Run, card: trello.Card)`                         | the run itself                                  |
| `on_claim proc(card: trello.Card)`                                      | once the card is claimed, before the run        |
| `on_done proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)` | after a run that finished                       |
| `on_park proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)` | after a run that ended waiting on a human reply |
| `on_fail proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome)` | after a run that failed                         |

`run` is an `afkd.Run` (`id`, `scratch_dir`) and `outcome` an `afkd.Outcome` (`duration`,
`error`). `card` is a **`trello.Card`**, the plugin's handle for the card, the same handle
[`trello.cards`](#the-reads) hands back:

| Field              | Type           | What it is                                         |
|--------------------|----------------|----------------------------------------------------|
| `id`               | `str`          | the card's short link, `Qb7eLy2w`                  |
| `key`              | `str`          | `trello:<Trello's 24-character id>`, for every card, claimed or read |
| `title`            | `str`          | the card's name, the same text as `name`, kept beside it until the actions above are removed |
| `number`           | `int`          | Trello's `idShort`, the number in the card's address |
| `name`, `url`      | `str`          | the card's name and its address on trello.com      |
| `description`      | `str`          | markdown, `""` when empty                          |
| `board`            | `str`          | the board's short link                             |
| `list`             | `str`          | the name of the list the card is in                |
| `labels`           | `list[str]`    | the names of the labels on the card                |
| `members`          | `list[str]`    | the usernames of the members on the card           |
| `due`              | `time.Time?`   | `nil` for a card with no due date                  |
| `due_complete`     | `bool`         | whether the due date is marked complete            |
| `archived`         | `bool`         | Trello's `closed`                                  |
| `created_at`, `last_activity_at` | `time.Time` | `created_at` from the card's id, as Trello's docs say |

A config passes a card around and compares it, but never builds one. The plugin reads every
field once the claim is won — after a parked card's `Awaiting Reply` badge has come off and
the claim comment is on it — and they hold for the whole run: `list` is where the claim
found the card, whatever `on_claim` then does, and `labels` leaves out a colour-only label,
which has no name. The `key` is the card's own, so a claimed card and the same card read
with `trello.cards` are `==`; the claim comment and the list a card was claimed from, which
the key no longer carries, are the plugin's [to keep](#the-claim-the-key-no-longer-carries).

afkd runs a slot's statements in the order they are written, and each action it calls is
one request to the plugin, which does it on the card the action is passed first:

| Action                                    | Effect                                  |
|-------------------------------------------|-----------------------------------------|
| `trello.add_member(card, <member>)`       | add a member to the card; additive and idempotent |
| `trello.remove_member(card, <member>)`    | remove a member from the card; idempotent (removing a member not on the card is a no-op) |
| `trello.mark_complete(card)`              | mark the card complete                  |
| `trello.move_to(card, "<list>", at=.top)` | move the card to a list, at `.top` (the default) or `.bottom` |
| `trello.remove_label(card, "<name>")`     | remove a named label from the card; idempotent (removing a label not on the card is a no-op) |
| `trello.add_label(card, "<name>")`        | add a named label to the card           |
| `trello.comment(card, "<text>")`          | post a comment on the card, verbatim; in a post-run slot, `#{outcome.duration}` interpolates how long the run took |
| `trello.attach(card, "<path>", "<name>")` | upload the file at `<path>` to the card as an attachment named `<name>` |
| `trello.fetch_attachment(card, "<name>", "<to>")` | write the newest file on the card named `<name>` to `<to>`; nothing when there is none |
| `trello.archive(card)`                    | archive the card                        |

The card is a `trello.Card` and nothing else: `trello.move_to("Review")`, with no card, or
passed another plugin's handle, is refused when the config loads.

A `<member>`, in a call or a setting, is a username or **`"me"`**, Trello's own alias for
the member the plugin's credentials authenticate as; it cannot name a real member, since a
Trello username is at least three characters long. `add_member` and `remove_member`
default it to `"me"`, so `trello.add_member(card)` adds that member.

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

**Files on the card.** A comment holds at most 16 384 characters, so a plan, a review or a
patch goes on the card as a file. `attach` reads the file at `path` when the slot calls it
and uploads it under `name`, at most 10 MiB; a file it cannot read, or a larger one, fails
the call. It is a write like the others: when Trello cannot take it, the bytes it read are
queued and retried, so a file the slot then moves or deletes still lands as it was. If an
upload landed but Trello answered with a 5xx, the retry puts a second, identical copy on the
card. `fetch_attachment` writes the newest file on the card named exactly `name` to the
path `to` — a link of that name is not a file and is passed over — and when there is none it
writes nothing and leaves a file already at `to` as it was. It writes through a temp file
beside `to`, so `to` never holds half a download, and it does not create `to`'s directory.
A fetch is a read the slot needs now, so it is never queued: when Trello cannot be reached
it fails with the reason, and an `attach` still queued behind an outage is not on the card
yet. Both paths must be absolute, since the plugin does not run in the slot's `work_dir`.

```conf
import "afkd"
import "@afkd/trello"

develop :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "Up for Grabs"

  on_claim proc(card: trello.Card) {
    match trello.fetch_attachment(card, "plan.md", "/srv/acme/widgets/earlier-plan.md") {
      .ok(_) => { }
      .err(e) => fail("trello.fetch_attachment: #{e.message}")
    }
  }
  on_fail proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.attach(card, "#{run.scratch_dir}/plan.md", "plan.md")
    trello.comment(card, "the attempt failed; its plan is attached as plan.md")
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }
}
```

**Repeating an action.** The label and member actions are all idempotent, so a retried card
cannot fault on its own slots: `add_label` and `add_member` are additive — attaching
what is already attached is a no-op — and `remove_label` and `remove_member` succeed
against a card that never carried the label or the member. The asymmetry to know is
board-level: `add_label` **creates** the named label on the board when it does not exist
yet, while `remove_label` never does (a name the board does not know simply has nothing to
remove); a *member* who is not on the board is an error either way. `move_to` is the one
action whose effect is positional rather than additive — it re-lists the card at the
target's `top` or `bottom` — so two `move_to`s leave the card wherever the last one put it.

**Cadence.** The plugin checks the board for work the moment the service starts, then
`every` after each check that claimed nothing or whose card failed; a card run to a finish
is followed by the next check at once. It fires for cards in `pick_from`'s lists; a
service works **one card at a time, to completion**, and `max_attempts` bounds the retries
per card. The slots move, complete, or archive the card as work starts, finishes, or fails.

**Intake.** `pick_from` names one list or several, `pick_from "To Do"` or
`pick_from "Ideas", "To Do"`. The poll goes through them in the order written, each from
the top, and claims the first card that passes every gate below; the gates mean the same in
every list. A name the board has no list for fails the poll, naming it. A card the plugin
puts back — its run crashed, or afkd handed it back unrun — goes to the bottom of the list
it was claimed from, so a card that keeps crashing cannot starve the head of the queue. A
parked card claimed again once answered goes back to the list it sat in when that is one of
`pick_from`'s, and otherwise to the first.

By default the poll claims the first card it comes to, the head of the first list that
holds one. With
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
import "afkd"
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

  on_claim proc(card: trello.Card) {
    trello.move_to(card, "In Progress", at=.top)
    trello.remove_label(card, "ready")
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
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
import "afkd"
import "@afkd/trello"

groom :: service(trello) {
  board        "https://trello.com/b/REPLACE_ME/board"
  api_key      "REPLACE_ME"
  token        "REPLACE_ME"
  pick_from    "Ideas"
  discuss_with [ "alice", "bob" ]

  on_claim proc(card: trello.Card) { trello.add_member(card) }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }
}
```

A board whose lists are all worked alike needs one service for all of them: `pick_from`
names each list, and this one discusses every card where it sits, in either list, with no
moves.

```conf
import "afkd"
import "@afkd/trello"

discuss :: service(trello) {
  board        "https://trello.com/b/REPLACE_ME/board"
  api_key      "REPLACE_ME"
  token        "REPLACE_ME"
  pick_from    "Bakgrundsvideor", "Hjälpvideor 🎬"
  discuss_with [ "anyone" ]

  work_dir "/srv/acme/videos"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
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
is only for extras (`on_park proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Discussion") }` for a board
that wants parked cards gathered somewhere).

The badge is also how afkd **finds the card again**. Once a beat, before it reads
`pick_from`, the plugin asks the board which of its open cards carry `Awaiting Reply`; a
badged card whose thread has gained a comment afkd did not write, newer than afkd's own last
word, is claimed again right there — wherever it sits on the board, ahead of anything queued
— the badge comes off, and the reply arrives under **New comments** in the fresh brief. Put
back, it returns to that list when `pick_from` names it, and to the first list otherwise. A
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
import "afkd"
import "ai:claude"
import "std:fs"
import "@afkd/trello"

// The agent the service below calls. Minimal, so this snippet validates on its own.
builder :: agent(claude) {
  model  "sonnet"
  skills "@afkd/trello/trello"
}

widgets :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "To Do"

  on_claim proc(card: trello.Card) { trello.move_to(card, "In Progress", at=.top) }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    match builder("build the card; ask through the trello skill if unclear") {
      .ok(_) => { }
      .err(e) => fail(e.message)
    }
    if fs.is_file("#{run.scratch_dir}/park") { fail("parked: awaiting a human reply") }
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
`on_claim proc(card: trello.Card) { trello.move_to(card, …) }`, as the example below does. A human who
drags a badged card back into the parking service's own `pick_from` gets it claimed and
unbadged there, answered or not.

```conf
import "afkd"
import "@afkd/trello"

widgets :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "To Do"

  on_claim proc(card: trello.Card) { trello.move_to(card, "In Progress", at=.top) }
  on_done proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Review", at=.top) }
  on_fail proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) { trello.move_to(card, "Backlog", at=.bottom) }
  on_park proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.comment(card, "parked after #{outcome.duration} — waiting on you")
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }
}
```

**Talking to a run in progress.** Everything above happens *between* runs: the brief is a
snapshot taken at claim time, so a comment posted while the agent is working would be
invisible to the run it is about — and on a board whose `on_done` archives the card,
invisible for good. afkd closes that window for every service, with no setting: it asks the
plugin for the claimed card's comments every 25-35 s for the extent of the run, and once
more when the last agent stops, and hands any new one to the agent **that is already
working**, as another turn in the same conversation. Nothing in the block asks for it:

```conf
import "afkd"
import "@afkd/trello"

widgets :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   "REPLACE_ME"
  token     "REPLACE_ME"
  pick_from "To Do"
  every     2m~3m

  on_claim proc(card: trello.Card) { trello.move_to(card, "In Progress", at=.top) }
  on_done proc(run: afkd.Run, card: trello.Card, outcome: afkd.Outcome) {
    trello.mark_complete(card)
    trello.move_to(card, "Done", at=.bottom)
    trello.archive(card)
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }
}
```

Every delivered message is **also appended to the run's `task/README.md`**, so the next agent of a
multi-agent run reads the correction in the task itself, and a retry keeps it. Comments are
delivered once each, oldest-first, attributed exactly as a brief's `## New comments` section
is; afkd's own comments and claim markers are never delivered back, and the comments the
brief was built from are never delivered again. Cost: one comment read every 25-35 s **per
running card**, plus one at the end of each run, and nothing at all while no run is in
flight. All of this is afkd's own mid-run watch; the plugin only reads the thread for it.

## Reading Trello through a `trello.Account`

Beside the card a service claims, a config reads a whole board: its cards, a card's comments,
checklists, check items and attachments, the member the token acts as, the boards it
reaches, and a board's lists, labels and members, and it saves an attachment to a path. Each
read takes a `trello.Account` first, the credential it reaches Trello with, and reaches
Trello through nothing else: no read takes a service's settings, so it reads the same from
an interval service's run, from another plugin's service and from a `service(trello)`'s
slots. Called outside the plugin's own services, a read is a child afkd starts for that one
call.

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/trello"

ACME :: trello.Account{ api_key: env.get("TRELLO_API_KEY") ?? fail("TRELLO_API_KEY is unset"), token: env.get("TRELLO_TOKEN") ?? fail("TRELLO_TOKEN is unset") }

standup :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match trello.cards(ACME, "https://trello.com/b/REPLACE_ME/board", list=[ "Review", "Blocked" ], without_labels=[ "Hold" ], members=[ "me" ], limit=20) {
      .ok(cards) => {
        for card in cards {
          $ echo #{card.id} #{card.list} #{card.name}
          if card.due != nil && !card.due_complete {
            $ echo #{card.id} is due and not done
          }
        }
      }
      .err(e) => fail("trello.cards: #{e.message}")
    }
  }
}
```

### `trello.Account`

A struct the plugin declares, built as `trello.Account{ … }`:

| Field      | Type                                | What it is                                      |
|------------|-------------------------------------|-------------------------------------------------|
| `api_key`  | `str`, secret                       | the Trello app's key                            |
| `token`    | `str`, secret                       | the token the calls act with                    |
| `base_url` | `str = "https://api.trello.com/1"`  | a stand-in's address in tests                   |

- An empty `api_key` or `token` makes every call an `.err` naming the field, `cards:
  acct.token is empty`, before anything is sent. Two accounts are two `Account` values, used
  side by side.
- `"me"` is resolved once per account and kept, for as long as the process that answers the
  calls lives, since Trello allows only 100 requests to `/1/members` per 900 seconds. A
  service's trigger child answers every read called in its slots, so it asks once per account;
  a read called outside a service is a child of its own, which asks once.
- The credential is sent only to `base_url`'s origin and, for a download, to trello.com,
  where Trello's own attachments live. A link to anywhere else is never fetched.

### The handles

Every handle is sent with every field, filled: one that may hold nothing is `T?` and `nil`
when it does, and a list with nothing in it is empty. Its `key` is `trello:<Trello's id>`, so
a handle read twice, or claimed and read, is `==`, and a read finds the object by it.

| Handle              | Fields |
|---------------------|--------|
| `trello.Card`       | `id` (the short link), `key`, and the fields [above](#servicetrello-cards) |
| `trello.Comment`    | `id` (Trello's id for the action), `card` (the card's short link), `body`, `author: str?` (a username), `created_at: time.Time`, `edited_at: time.Time?` |
| `trello.Checklist`  | `id` (Trello's id), `card`, `name`, `items: list[str]` (the items' names, in order) |
| `trello.Check_Item` | `id` (Trello's id), `card`, `checklist` (its id), `name`, `done: bool` |
| `trello.Attachment` | `id` (Trello's id), `card`, `name`, `url`, `bytes: int?`, `mime_type: str?`, `created_at: time.Time`, `is_upload: bool` (a file Trello holds, not a link) |
| `trello.Board`      | `id` (the short link), `name`, `url`, `archived: bool` |
| `trello.List`       | `id` (Trello's id), `name`, `board` (the short link), `archived: bool` |
| `trello.Label`      | `id` (Trello's id), `name` (`""` for a colour-only label), `color: str?`, `board` |
| `trello.Member`     | `id` (the username), `full_name` |

An **attachment's key** is `trello:<card id>/<attachment id>`, the one key that holds two ids:
Trello has no route to an attachment outside its card, and a handle crosses the wire as its
`id` and `key` alone, so `download_attachment` could reach the file by nothing else. A card's
comment is found by its action id alone.

### The reads

| Call                                                | Gives |
|-----------------------------------------------------|-------|
| `trello.cards(acct, board, ~filters…)`              | `list[trello.Card]` of the board matching every filter given, list by list in the board's order, each list top to bottom. The plugin reads the whole board, paging past Trello's 1000 cards, then filters, orders and cuts at `limit` |
| `trello.comments(acct, card, ~limit = 200)`         | `list[trello.Comment]`: the newest `limit` (1 to 1000), oldest first, paging Trello's actions with `before` |
| `trello.checklists(acct, card)`                     | `list[trello.Checklist]`, in the card's order |
| `trello.check_items(acct, card)`                    | `list[trello.Check_Item]`, checklist by checklist, each in order |
| `trello.attachments(acct, card)`                    | `list[trello.Attachment]`, oldest first, files and links |
| `trello.download_attachment(acct, attachment, to)`  | nil; saves an uploaded file of at most 10 MiB to the absolute path `to` through a temp file beside it, sending the `Authorization` header Trello's download asks for |
| `trello.me(acct)`                                   | the `trello.Member` the token acts as |
| `trello.boards(acct)`                               | `list[trello.Board]` the token can reach, open ones |
| `trello.lists(acct, board)`                         | `list[trello.List]`, open ones, in the board's order |
| `trello.labels(acct, board)`                        | `list[trello.Label]`, all of them: Trello gives 50 unless asked, so the plugin asks for 1000 |
| `trello.members(acct, board)`                       | `list[trello.Member]` |

A **board** is named by its address or its short link, `"https://trello.com/b/Qb7eLy2w/widgets"`
or `"Qb7eLy2w"`; a card, a comment, a checklist or an attachment is passed as the handle a read
gave back, or the card a service claimed; a **list** or a **label** by its name on the board,
and a **member** by username or `"me"`, each matched exactly. `trello.card(acct, id)`, a read of
one card by its id, is not offered: the kind is named `card`.

### `trello.cards`'s filters

All are named-only and may be left out.

| Filter             | Type            | Keeps |
|--------------------|-----------------|-------|
| `list`             | `list[str]`     | cards in any of these lists |
| `labels`           | `list[str]`     | cards carrying every one |
| `without_labels`   | `list[str]`     | cards carrying none |
| `members`          | `list[str]`     | cards any of these members is on, `"me"` allowed |
| `include_archived` | `bool = false`  | archived cards and the cards of archived lists too |
| `limit`            | `int = 100`     | 1 to 1000 |

- A list, label or member the board has none of is an `.err` naming the ones there are,
  `cards: the board has no label named "Hold"; its labels are "afkd/ready", "Problem"`. A list
  name two lists share is an `.err` listing their ids, `cards: 2 lists are named "Doing" (ids
  <id>, <id>); name one of them by renaming the other`. Both are found before any card is
  read. An archived list is named only with `include_archived`.
- A filter that names nothing is refused rather than read as keeping nothing, ``cards: `list`
  is empty; leave it out to keep every card``, and so is a `limit` outside 1 to 1000, as is a
  `comments` `limit`.
- A card with only a colour-only label has no label names, so `labels` never matches it and
  `without_labels` never excludes it.

### Refusals

Every read is done or refused: `.ok` means Trello answered, and an `.err` carries one
sentence, which names the call.

- An empty `api_key` or `token`: `cards: acct.api_key is empty`.
- A board Trello does not have: `lists: no board "Nowhere1" on this account: trello read
  board: board returned status 404`; a card or an attachment it does not have likewise.
- A handle this plugin did not hand out, such as a key of another shape: `comments: card
  Qb7eLy2w has a key this plugin did not hand out: <key>`.
- `download_attachment` of a link, `attachment "the spec" is a link to https://example.com/spec,
  not a file Trello holds`, of a file over 10 MiB, `attachment "huge.bin" is 10485761 bytes,
  over the 10 MiB the plugin fetches`, to a path that is not absolute, or to one it cannot
  write: each before or without a partial file.
- A reply over afkd's 8 MiB for one call. `cards` and `comments` say which `limit` to lower,
  `cards: the reply comes to 8912345 bytes, over afkd's 8 MiB for one call; lower limit`, and
  every other read says that its reply is too large.

**Retries.** A read is done now or refused; nothing is queued. A request Trello answers
with a 429 is tried again, on every verb, and one answered with a 5xx on every verb but
`POST`, which Trello may already have carried out. Each wait is the largest of the
`x-rate-limit-*-interval-ms` headers the answer carries, or, when it carries none, one
second, then two, doubling. A wait that would end past the call's deadline, 45 seconds, is
not taken: the call is refused with the last answer it got. The writes of the kind, the
actions on a claimed card, are not retried this way: the outbox retries those.

### Two accounts, and from a slot

A read called in a slot passes the account a config builds, never the service's own settings,
so two accounts, one per Trello, are read side by side:

```conf
import "afkd"
import "std:env"
import "@afkd/trello"

ACME   :: trello.Account{ api_key: env.get("TRELLO_API_KEY") ?? fail("TRELLO_API_KEY is unset"), token: env.get("TRELLO_TOKEN") ?? fail("TRELLO_TOKEN is unset") }
GLOBEX :: trello.Account{ api_key: env.get("GLOBEX_TRELLO_API_KEY") ?? fail("GLOBEX_TRELLO_API_KEY is unset"), token: env.get("GLOBEX_TRELLO_TOKEN") ?? fail("GLOBEX_TRELLO_TOKEN is unset") }

develop :: service(trello) {
  board     "https://trello.com/b/REPLACE_ME/board"
  api_key   env.get("TRELLO_API_KEY") ?? fail("TRELLO_API_KEY is unset")
  token     env.get("TRELLO_TOKEN") ?? fail("TRELLO_TOKEN is unset")
  pick_from "Up for Grabs"

  on_claim proc(card: trello.Card) {
    match trello.comments(ACME, card, limit=50) {
      .ok(comments) => {
        for comment in comments {
          $ echo #{comment.author ?? "nobody"}: #{comment.body}
        }
      }
      .err(e) => fail("trello.comments: #{e.message}")
    }
    match trello.attachments(ACME, card) {
      .ok(files) => {
        for file in files {
          if file.is_upload {
            match trello.download_attachment(ACME, file, "/srv/acme/widgets/#{file.name}") {
              .ok(_) => { }
              .err(e) => fail("trello.download_attachment: #{e.message}")
            }
          }
        }
      }
      .err(e) => fail("trello.attachments: #{e.message}")
    }
    match trello.boards(GLOBEX) {
      .ok(reachable) => {
        for other in reachable {
          $ echo #{other.id} #{other.name}
        }
      }
      .err(e) => fail("trello.boards: #{e.message}")
    }
    trello.move_to(card, "In Progress", at=.top)
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, card: trello.Card) {
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }
}
```

The `card` a read is passed may be the one the service claimed or one a read handed back:
both are `trello:<card id>`.

### The claim the key no longer carries

A claimed card's key was `<card id>#<claim comment id>`, with the list it was claimed from
after a second `#` when `pick_from` names several. It is `trello:<card id>` now, the key a read
of the card hands out, so the two are one handle. The claim comment, which a release deletes,
and the list the card returns to are the plugin's own **record**, kept in memory and, when afkd
names the plugin a directory to keep state in, in a file there, `claims/<card id>.json`,
written before the poll's reply and removed once the claim is finished or released. A process
afkd restarts reads it back, so a card claimed from a later list still returns to that list,
and a key an older release journaled, `card#claim` or `card#claim#list`, is still released as
it was.

A `trello:` key whose record is gone — the directory is not kept, or the card was claimed
before the record was — is released by a rule that holds without one: every `[afkd-claim]`
comment on the card posted by the member the token acts as is deleted, except one carrying this
process's current claim owner, which is a live claim of this process; a claim by any other
member is kept; and the card goes to the bottom of the `pick_from` list it sits in, or the
first. Another afkd process on the same Trello account posts claims under the same member with
an owner of its own, so this rule can delete theirs. Such a claim can only be on the card if
the one the plugin holds had lapsed — a live claim is stepped over, so the other process
could only claim after the first went an hour unrenewed — and then the other run meets a 404
on its next renewal, which it diagnoses, and the card returns to `pick_from`. **Give each afkd
process its own Trello account**: one account is one identity to the kind, which reads every
comment by the token's member as afkd's own in the brief, the feedback delta and the
backstop alike.

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **afkd runs the slots.** The built-in ran its lifecycle blocks itself, in a fixed order
  and inside its own claim and finish. Here a slot is code afkd runs in the order it is
  written, and each action is one request to the plugin naming the card it acts on, sent
  once the plugin's own claim or finish has landed: `on_claim` after the claim is won, and
  the post-run slot after the claim is released — so a card moved by `on_done` is, for a
  moment, unclaimed in its old list. afkd retries no slot's action.
- **`me`, not `self`.** The member the credentials authenticate as is `"me"` in a call or a
  setting — Trello's own alias for it — where the built-in reserved the word `self`; a
  member action left without one adds or removes that member.
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
  poll, and each claim attempt waits three seconds for a rival's marker to show, so a scan
  that keeps losing races stops after seven and leaves the rest of the list for the next
  poll. And every call the plugin answers comes back within 45 seconds: a board too slow to
  answer in that time is treated as if it were down.
- **afkd frames the brief as a plugin's.** A run's `task/README.md` opens `# Work item from plugin
  card Qb7eLy2w` where the built-in's opens `# Work item from trello card Qb7eLy2w`, and a
  comment delivered mid-run calls the card "this work item" rather than "this card".
- **The claim key is the card's.** The built-in journaled `card#claim[#list]`; the plugin
  journals `trello:<card id>` and keeps the claim comment and the list in a
  [record](#the-claim-the-key-no-longer-carries) of its own. A key the built-in or an older
  release journaled is still released, as it was.
- **One Trello account per afkd process.** Two afkd processes on one account are one
  identity to the plugin: a release with no record prunes every claim the account's member
  posted but the releasing process's own.
