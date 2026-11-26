# `@afkd/afkd_board`

A **provider** plugin for a board on [app.afkd.sh](https://app.afkd.sh), with one trigger kind,
[`service(afkd_board)`](#serviceafkd_board): it claims the board's cards, one per run, and passes
the service's slots the claimed card and an [`afkd_board.Board`](#afkd_boardboard). The same board
is read and written from any config: one card by its number, the cards any filters keep, a card's
comments and its checklist, and the board's lists and labels, and
[every write](#writing-to-a-board) of its cards, checklists, comments and lists, each write of a
card made under the run it is made in. Each call reaches the board through the
`afkd_board.Board` it is passed, by one of the board's API keys, so a config works two boards side
by side with two of them. The plugin also ships the `afkd_board` [skill](#the-skill) an agent
works the card through, one script per operation. Every call a config makes outside a service is
a child afkd starts for that one call.

It reads the board's HTTP API, which the site's README documents (*A board's API*). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[the afk language](https://afkd.sh/docs/reference/) for the configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/afkd_board
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest, the `afkd-afkd_board` binary, its `skills/` and the `LICENSE-MIT` and
`LICENSE-APACHE` texts, nothing else. Nothing is compiled on your host, and no Rust toolchain is needed
there. Installing it needs **afkd 0.2.714 or newer**, the first whose manifests take an
`afkd.Run?` on an action's parameter, as every write's `run` is; every older floor (a handle's
field typed as a `time`, a parameter typed as optional) comes with it. `afkd update
@afkd/afkd_board` moves an installed copy onto the next release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/afkd_board/Cargo.toml
$ tools/pack-release afkd_board /tmp/afkd-release @afkd/afkd_board/target/release/afkd-afkd_board
$ afkd install /tmp/afkd-release/afkd-afkd_board
```

The configs below are written in the afk language of **afkd 0.2.714 or newer**, which reads
`env.get(…) ?? fail(…)` in a constant at load (0.2.694 was the first) and passes `run=run` to an
action; an older afkd refuses the examples when they load.

## `service(afkd_board)`

A service of the plugin's one kind claims a board's cards, one per run, the way afkd claims on its
other trackers: the `afkd/claimed` label goes on the card, and the claim goes into afkd's claim
journal. Its slots are passed the claimed card and the `afkd_board.Board` the service's `key` and
`base_url` build, so each reaches the board [through it](#reading-a-board-through-an-afkd_boardboard),
as a config does anywhere else:

```conf
import "afkd"
import "std:env"
import "@afkd/afkd_board"

develop :: service(afkd_board) {
  key           env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset")
  claim_from    "Todo"
  skip_labels   "blocked"
  only_blocked  false
  every         1m
  max_attempts  2

  on_claim proc(b: afkd_board.Board, card: afkd_board.Card) {
    match afkd_board.card(b, card.number) {
      .ok(read) => {
        if read != card {
          fail("the card read is not the card claimed")
        }
      }
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card) {
    match afkd_board.move_card(b, card, "In progress", at=.top, run=run) {
      .ok(_) => { }
      .err(e) => fail("afkd_board.move_card: #{e.message}")
    }
    $ cat $AFKD_SCRATCH_DIR/task/README.md
  }

  on_done proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card, outcome: afkd.Outcome) {
    match afkd_board.move_card(b, card, "In review", run=run) {
      .ok(_) => { }
      .err(e) => fail("afkd_board.move_card: #{e.message}")
    }
  }

  on_park proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card, outcome: afkd.Outcome) {
    match afkd_board.create_comment(b, card, "parked: waiting for a reply", run=run) {
      .ok(_) => { }
      .err(e) => fail("afkd_board.create_comment: #{e.message}")
    }
  }

  on_fail proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card, outcome: afkd.Outcome) {
    match afkd_board.add_label(b, card, "blocked", run=run) {
      .ok(_) => { }
      .err(e) => fail("afkd_board.add_label: #{e.message}")
    }
  }
}
```

The key comes from the daemon's environment, so it never sits in the file. A service with no
`claim_from` is refused when the config loads.

### Settings

| Setting        | Type                | What it is                                                                 |
|----------------|---------------------|----------------------------------------------------------------------------|
| `key`          | `str`, required     | one of the board's API keys, `afk_board_…`, as [`Board`'s](#afkd_boardboard) field; an empty one is refused when the service arms |
| `base_url`     | `str = "https://app.afkd.sh"` | a stand-in's address in tests, as `Board`'s field              |
| `claim_from`   | `list[str]`, required | the lists to claim from, in the order written, each top to bottom; one named twice is refused |
| `only_labels`  | `list[str]`         | it claims only a card carrying every one                                   |
| `skip_labels`  | `list[str]`         | it never claims a card carrying any of them, and wins over `only_labels`   |
| `only_blocked` | `bool`              | `false` claims only a card no open card blocks, `true` only a blocked one; left out, it claims either |

`every` and `max_attempts` are afkd's own for every service that claims, and are written beside
these. Lists and labels are matched by name, exactly; a `claim_from` list the board lacks makes the
beat idle, saying which, and so does a board that cannot be reached.

The slots are afkd's, each passed `b` and `card` and, after the run exists, the `run` first and the
run's `outcome` last:

| Slot | Passed |
|------|--------|
| `on_claim proc(b: afkd_board.Board, card: afkd_board.Card)` | once the card is claimed, before the run |
| `on_run proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card)` | the run itself |
| `on_done`, `on_park`, `on_fail` `proc(run: afkd.Run, b: afkd_board.Board, card: afkd_board.Card, outcome: afkd.Outcome)` | after a run that finished, parked or failed |

The claimed card is `==` the card `afkd_board.card` reads of the same number, since `==` compares the
handle's key, `afkd_board:<board uuid>/<card uuid>`; the unit's `id` is the card's number. The
unit's brief, `task/README.md`, holds the card's title, description, checklist and comments, the
claim markers left out, and is cut to fit afkd's 64 KiB plugin line.

### How a card is claimed

The board has no claim call, so the claim is `@afkd/linear`'s, built on what the board answers in an
order every reader sees alike: the card's comments.

1. **The walk.** Each poll claims at most one card. It walks `claim_from` in the order written, each
   list top to bottom, and passes over a card that is archived, done, already carrying
   `afkd/claimed`, or held back by `only_labels`, `skip_labels` or `only_blocked`.
2. **The marker.** For each card left it posts a comment `[afkd-claim] owner=<owner>`, the afkd
   process's claim owner, as the board's key.
3. **The settle.** It waits 3 s, so a rival's marker has time to become visible, and then reads the
   card's comments and the card again.
4. **The decision.** The poll wins iff its marker is the unique minimum of `(created_at, id)` among
   the markers a key made, no more than **90 s** before its own, and the card still lacks
   `afkd/claimed`. Both contenders compute it from the same list, so exactly one wins. A comment a
   person wrote that looks like a marker is no marker: only a key's comments count. The board stamps
   a comment to the second, and a comment's id is a uuid, which carries no time, so within one
   second the id's order decides: arbitrary, but the same everywhere. A loser moves on to the next
   card in the same poll.
5. **The label.** The winner puts `afkd/claimed` on the card and hands afkd the unit. The label marks
   the claimed card for people, and for other services' `skip_labels`; a card carrying it is never
   claimed by another instance, on this machine or another. If the label cannot be put on, the beat
   is idle and says why.

**Why 90 s is safe.** A marker cannot be deleted or edited on the board, so it cannot be renewed or
released as a claim on other trackers is; it ages out of the decision instead. A marker older than
the window cannot beat a newer one, and the winner of a race puts the label on within its poll's 45 s
budget, long before any marker that raced for the card is 90 s old: every poll that reads the card
after that sees the label and stands down, whatever the markers say. A marker is only ever read as a
rival's by a poll that began within 90 s of it. No poll starts a new race with less than 10 s of the
45 s left.

**The claim journal and the label come off.** afkd journals the claim under the unit's key. The
label must come off only after `on_done`, `on_park` or `on_fail` has run, and afkd's beat is `poll`,
the pre-run slot, the attempts, `finish`, the post-run slot, the next beat, so `finish` cannot take
it off: it writes the card down as owed a release in `owed-releases.json` under the plugin's
directory, and the child's **next `poll` takes the label off** before it claims anything. Every
`service(afkd_board)` in a home, and every pool copy of one, shares that directory and afkd's
identity, so each entry names its writer (the board, afkd's claim owner and the child's own pid) and
the file is changed only under a lock, written whole. A child takes off only its own entries for its
own board, and the entries for its board that an earlier afkd process left. A `finish` that cannot
write the entry answers `held`, and afkd's next beat sends `release` for the key.

**After a crash** the label stays on, and the claim journal still holds the key: the next start's
`release` of the key takes the label off, and answers `true` also for a card that no longer carries
it. A key of another board, or of no shape, is `null`. Until then no other instance claims the
card.

**The run a change is shown under.** A unit's `env` is fixed when afkd polls, before afkd makes
the run, so it cannot name the run: it carries `AFKD_BOARD_URL`, `AFKD_BOARD_AUTHORIZATION` and
`AFKD_BOARD_RUN`, the JSON `{"service": "<the service>"}`. A run carries its id as the leaf of
`$AFKD_SCRATCH_DIR` and its copy's index as `$AFKD_INSTANCE`, but its machine, the organization
it joined or else the host's name, is in no variable. So the [skill's](#the-skill) scripts send no
`run` yet, and say so once on stderr: the board shows an agent's changes under the key's name. A
follow-up asks afkd to name a run's `machine`, and its `service`, which differs from the
`service` afkd sends the plugin for a pool copy, in variables a run carries; the scripts send the
run once they are told. A slot's own writes pass `run=run` and are shown under the run already.

## The skill

The plugin ships the `afkd_board` skill, under `skills/afkd_board/`: an agent in a
`service(afkd_board)` run works its card through scripts, one for each operation of the plugin
under the operation's name, each printing as JSON what the operation gives back: `card.py`,
`cards.py`, `comments.py`, `checklist.py`, `lists.py`, `labels.py`, `create_card.py`,
`update_card.py`, `move_card.py`, `add_label.py`, `remove_label.py`, `add_item.py`,
`check_item.py`, `remove_item.py`, `mark_done.py`, `add_blocked_by.py`, `remove_blocked_by.py`,
`create_comment.py`, `archive_card.py`, `unarchive_card.py`, `delete_card.py`, `create_list.py`,
`update_list.py` and `delete_list.py`. They reach the board through the run's `AFKD_BOARD_URL` and
`AFKD_BOARD_AUTHORIZATION`, run with `python3`, and are documented, argument by argument, in the
skill's `SKILL.md`. The `afkd/claimed` label is the service's, and the skill tells the agent to
leave it alone.

## Reading a board through an `afkd_board.Board`

A config file uses the plugin by importing it, and then names it by its leaf, `afkd_board`. Each
read takes the `afkd_board.Board` first, then what it reads by, and gives a `result`: `.ok` of
what it read, or `.err` with the reason it was refused. Nothing else is read: no read takes a
service's settings, so it reads the same from any service's run.

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset") }

standup :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match afkd_board.cards(
      TRAIN,
      list=[ "Todo", "In progress" ],
      without_labels=[ "hold" ],
      done=false,
    ) {
      .ok(cards) => {
        for card in cards {
          for blocker in card.blocked_by {
            $ echo "#{card.id} #{card.title} is blocked by #{blocker}"
          }
        }
      }
      .err(e) => fail("afkd_board.cards: #{e.message}")
    }
  }
}
```

### `afkd_board.Board`

A struct the plugin declares, built as `afkd_board.Board{ … }`:

| Field      | Type                          | What it is                                              |
|------------|-------------------------------|---------------------------------------------------------|
| `key`      | `str`, secret                 | one of the board's API keys, `afk_board_…`, made in its Board settings |
| `base_url` | `str = "https://app.afkd.sh"` | a stand-in's address in tests                           |

- An empty `key` is an `.err` of every read, sent before any request, naming the field:
  ``cards: the afkd_board.Board's key is empty; set key to one of the board's API keys, made in its Board settings``.
- A key reaches exactly one board, so two boards are two `Board` values, used side by side.
- Every handle a read hands back has the `key` `afkd_board:<board uuid>/<card or comment uuid>`,
  and `==` on two handles compares it, so one card read by `afkd_board.card` and by
  `afkd_board.cards` is `==` itself.

### `afkd_board.Card`

A handle, its `id` the card's number as its digits (`"17"` for `#17`):

| Field         | Type        | What it is                                                     |
|---------------|-------------|----------------------------------------------------------------|
| `number`      | `int`       | `17` for `#17`                                                 |
| `title`       | `str`       |                                                                |
| `description` | `str`       | markdown, `""` when it has none                                |
| `list`        | `str`       | the name of the list it is in                                  |
| `labels`      | `list[str]` | by name, in the order they were put on                         |
| `archived`    | `bool`      |                                                                |
| `done`        | `bool`      |                                                                |
| `blocked_by`  | `list[int]` | the numbers of the cards it is blocked by, done or not         |
| `blocking`    | `list[int]` | the numbers of the cards it blocks                             |
| `blocked`     | `bool`      | whether a card it is blocked by is not done                    |
| `url`         | `str`       | its address on the site                                        |
| `created_at`  | `time`      |                                                                |
| `updated_at`  | `time`      | the newest of when it was made, its changes and its comments   |

A card's checklist is read through [`afkd_board.checklist`](#the-reads), not from the handle:
afkd keeps structs out of a handle's fields, and an item, an [`afkd_board.Item`](#the-values),
is one.

### `afkd_board.Comment`

A handle, its `id` the comment's uuid:

| Field        | Type   | What it is                                                                 |
|--------------|--------|----------------------------------------------------------------------------|
| `card`       | `int`  | the number of the card it is on                                            |
| `body`       | `str`  | markdown                                                                   |
| `author`     | `str?` | the name of the person who made it; `nil` for a comment a key made, and once the person's account is deleted |
| `run`        | `str?` | the id of the run it was made in                                           |
| `created_at` | `time` |                                                                            |
| `url`        | `str`  | its card's `url`, since a comment has no address of its own                |

### The values

Plain structs, read with the board:

| Struct             | Fields                        |
|--------------------|-------------------------------|
| `afkd_board.List`  | `id: str`, `name: str`        |
| `afkd_board.Label` | `name: str`, `colour: str`    |
| `afkd_board.Item`  | `text: str`, `done: bool`: an item of a card's checklist, what `afkd_board.checklist` gives |

### The reads

| Call | Gives |
|------|-------|
| `afkd_board.card(b, number)` | `Card`, archived or not; a number the board has no card of is an `.err`, ``card: The board has no card #17.`` |
| `afkd_board.cards(b, ~list, ~labels, ~without_labels, ~done, ~blocked, ~text, ~include_archived, ~limit)` | `list[Card]` in the board's order, [filtered](#afkd_boardcardss-filters) |
| `afkd_board.comments(b, card)` | `list[Comment]`, oldest first |
| `afkd_board.checklist(b, card)` | `list[Item]` in the card's order, `[]` when it has none |
| `afkd_board.lists(b)` | `list[List]` in the board's order |
| `afkd_board.labels(b)` | `list[Label]` |

### `afkd_board.cards`'s filters

Each filter is named, and one left out keeps every card:

| Filter             | Type        | Keeps                                                   |
|--------------------|-------------|---------------------------------------------------------|
| `list`             | `list[str]` | a card in any of these lists, by name                   |
| `labels`           | `list[str]` | a card carrying every one of these labels               |
| `without_labels`   | `list[str]` | a card carrying none of these labels                    |
| `done`             | `bool?`     | `true` the cards that are done, `false` those that are not; `nil` both |
| `blocked`          | `bool?`     | `true` the blocked cards, `false` the free ones; `nil` both |
| `text`             | `str`       | a card with it in its title or its description, case aside |
| `include_archived` | `bool = false` | the archived cards too                               |
| `limit`            | `int = 50`  | at most this many, 1 to 250                             |

- The board's order is by each card's list's place, then its place in the list, a list's
  archived cards after its others and the last archived first.
- The API filters one list a query, so `cards` of several lists reads each, and with
  `include_archived` reads the archived cards and the others apart, then merges them in the
  board's order and keeps the first `limit`.
- `list=[]` keeps no card, and `labels=[]` and `without_labels=[]` filter nothing.

### Refusals

Every refusal is an `.err` whose message names the read, `<read>: <what is wrong>`:

- An empty key, before any request, as [above](#afkd_boardboard).
- A `limit` outside 1 to 250, before any request: ``cards: limit is 1 to 250, not 0``.
- A list the board does not have, before any card is read, naming the lists it has:
  ``cards: The board has no list `Doing`; it has `Todo`, `In progress` and `Done`.``. A label
  it does not have is refused by the board the same way.
- A card of another board than the `afkd_board.Board` reaches, passed to `comments` or `checklist`,
  naming both boards: ``comments: card is #17 of board <its board's uuid>, and the afkd_board.Board reaches board <the other's uuid>``.
  A card the board no longer has is the board's own ``checklist: The board has no card #17.``
- What the board refuses, with its own sentence: a key it does not know is
  ``lists: the board key is missing, unknown or revoked``.
- A board it cannot reach names the host, and never the key:
  ``cards: cannot reach the board at app.afkd.sh: …``.
- A reply over afkd's 8 MiB for one call: ``cards: the reply comes to … bytes, over afkd's 8 MiB for one call; ask for fewer with limit``.

## Writing to a board

A write takes the `afkd_board.Board` first, like a read, and gives a `result`: `.ok` of what it
wrote, or `.err` with the reason it was refused. Every write of a card, of its checklist and of a
comment takes `~run`, a named `afkd.Run?`: pass `run=run`, the run the slot was passed, and the
card's page on the board shows the change under that run, with the run's state and, if it fails,
its problem. A write left without a run, or given `nil`, sends none, and the board shows the
change under the key's name. A write of a list takes no run.

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset") }

triage :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    made := match afkd_board.create_card(
      TRAIN,
      "Todo",
      "Retry storm in the client",
      description="Seen on the nightly run.",
      labels=[ "bug" ],
      checklist=[ afkd_board.Item{ text: "Reproduce it", done: false } ],
      at=.top,
      run=run,
    ) {
      .ok(card) => card
      .err(e) => fail("afkd_board.create_card: #{e.message}")
    }
    match afkd_board.move_card(TRAIN, made, "In progress", run=run) {
      .ok(moved) => {
        $ echo "#{moved.id} is in #{moved.list}"
      }
      .err(e) => fail("afkd_board.move_card: #{e.message}")
    }
    match afkd_board.create_comment(TRAIN, made, "Picked up by #{run.id}", run=run) {
      .ok(comment) => {
        $ echo "commented #{comment.id} on #{comment.card}"
      }
      .err(e) => fail("afkd_board.create_comment: #{e.message}")
    }
  }
}
```

### The writes

| Call | Gives |
|------|-------|
| `afkd_board.create_card(b, list, title, ~description, ~labels, ~checklist, ~at, ~after, ~run)` | `Card` with the next number, in `list` at `at` or after `after`, at the bottom with neither; a label the board lacks is made in the next free colour, and each item of `checklist`, an [`afkd_board.Item`](#the-values), starts not done whatever its `done` says |
| `afkd_board.update_card(b, card, ~title, ~description, ~run)` | `Card` |
| `afkd_board.move_card(b, card, list, ~at, ~after, ~run)` | `Card`, in `list` at `at` (`.top`, `.bottom`) or right after `after`, a `Card`; with neither it goes at the bottom |
| `afkd_board.add_label(b, card, label, ~run)`, `afkd_board.remove_label(b, card, label, ~run)` | `Card`; a label the board lacks is made with the next free colour; removing one the card lacks, or the board does, is `.ok` |
| `afkd_board.add_item(b, card, text, ~run)`, `afkd_board.check_item(b, card, index, done = true, ~run)`, `afkd_board.remove_item(b, card, index, ~run)` | `Card`; `index` counts from 0 |
| `afkd_board.mark_done(b, card, done = true, ~run)` | `Card`; marking a blocked card done is an `.err` naming the cards that are not done |
| `afkd_board.add_blocked_by(b, card, other, ~run)`, `afkd_board.remove_blocked_by(b, card, other, ~run)` | `Card`, `done` as the board left it; a block that makes a chain back is an `.err` naming the chain |
| `afkd_board.create_comment(b, card, body, ~run)` | `Comment` |
| `afkd_board.archive_card(b, card, ~run)`, `afkd_board.unarchive_card(b, card, ~run)`, `afkd_board.delete_card(b, card, ~run)` | `Card`; `delete_card` gives the card as it was, read before it was deleted |
| `afkd_board.create_list(b, name, ~at, ~after)`, `afkd_board.update_list(b, list, ~name, ~at, ~after)`, `afkd_board.delete_list(b, list)` | `List`, `List`, nothing; `list` and `after` are list names; deleting a list with cards that are not archived is an `.err` naming the count |

A write that gives a `Card` or a `Comment` gives it as the board holds it once the write is done,
a handle like a read's, so the same card written and read is `==` itself.

- **The board's API takes no run to delete a card** (`DELETE /api/board/cards/<number>` takes no
  body), so a `run` given to `delete_card` does not reach the API.
- **`at` and `after`.** `at` is `.top` or `.bottom`; `after` is a `Card` of the list it goes in, or,
  for a list, the name of the list it goes after. Both together is an `.err` before any request:
  ``create_card: at and after cannot both be given``.
- **List names are matched exactly**, so `todo` is not `Todo`. A list the board lacks is an
  `.err` before anything is written, naming the lists it has:
  ``move_card: The board has no list `Doing`; it has `Todo`, `In progress` and `Done`.``.
- **A card of another board** than the `afkd_board.Board` reaches, passed as `card`, `other` or
  `after`, is an `.err` naming both boards, as it is for [`comments`](#refusals).
- **A `run` that is no `afkd.Run`** is an `.err`: ``update_card: parameter run must be an afkd.Run``.
  The plugin sends exactly the run's `machine`, `service`, `instance` and `id`, the `instance` as
  the string afkd gives it.
- **What the board refuses** comes back with its own sentence: a blocked card marked done,
  ``mark_done: Card #3 is blocked by #1 and #2, which are not done.``; a block that makes a chain
  back, ``add_blocked_by: Card #1 cannot be blocked by #3, since #3 is blocked by #2, which is
  blocked by #1.``; a list that holds cards, ``delete_list: The list `Todo` holds 3 cards that are
  not archived; move or archive them first.``; an item past the end of the checklist.
- **`remove_label` of a label the board lacks** is `.ok`: the board answers `400` for it, but the
  card lacks it all the same, so the plugin reads the card and gives it as it is, writing nothing.

Lists are written by name, and a list write takes no run:

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset") }

tidy :: service(interval) {
  every    1d
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match afkd_board.create_list(TRAIN, "Review") {
      .ok(list) => {
        $ echo "made #{list.name} (#{list.id})"
      }
      .err(e) => fail("afkd_board.create_list: #{e.message}")
    }
    match afkd_board.update_list(TRAIN, "Review", name="Reviewed", after="Todo") {
      .ok(_) => { }
      .err(e) => fail("afkd_board.update_list: #{e.message}")
    }
    match afkd_board.delete_list(TRAIN, "Reviewed") {
      .ok(_) => { }
      .err(e) => fail("afkd_board.delete_list: #{e.message}")
    }
  }
}
```

## Two boards side by side

Two keys are two boards, each an `afkd_board.Board` of its own. A card of one passed with the other
is refused, naming both. Each card's comments and its checklist's done items are read:

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset") }
OPS   :: afkd_board.Board{ key: env.get("AFKD_OPS_BOARD_KEY") ?? fail("AFKD_OPS_BOARD_KEY is unset") }

digest :: service(interval) {
  every    1d
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    for b in [ TRAIN, OPS ] {
      match afkd_board.lists(b) {
        .ok(lists) => {
          for list in lists {
            $ echo #{list.name}
          }
        }
        .err(e) => fail("afkd_board.lists: #{e.message}")
      }
      match afkd_board.cards(b, labels=[ "bug" ], include_archived=true, limit=250) {
        .ok(cards) => {
          for card in cards {
            match afkd_board.comments(b, card) {
              .ok(comments) => {
                for comment in comments {
                  $ echo "#{card.id} #{comment.author ?? "a key"} #{comment.run ?? "-"}"
                }
              }
              .err(e) => fail("afkd_board.comments: #{e.message}")
            }
            match afkd_board.checklist(b, card) {
              .ok(items) => {
                for item in items {
                  if item.done {
                    $ echo "#{card.id} done: #{item.text}"
                  }
                }
              }
              .err(e) => fail("afkd_board.checklist: #{e.message}")
            }
          }
        }
        .err(e) => fail("afkd_board.cards: #{e.message}")
      }
    }
  }
}
```

## One card, two reads

A handle read by two calls is `==` itself, since `==` compares the key:

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/afkd_board"

TRAIN :: afkd_board.Board{ key: env.get("AFKD_BOARD_KEY") ?? fail("AFKD_BOARD_KEY is unset") }

watch :: service(interval) {
  every    10m
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    lead := match afkd_board.card(TRAIN, 17) {
      .ok(card) => card
      .err(e) => fail("afkd_board.card: #{e.message}")
    }
    match afkd_board.cards(TRAIN, list=[ lead.list ]) {
      .ok(cards) => {
        for card in cards {
          if card == lead {
            $ echo "#17 is #{card.title}, in #{card.list}"
          }
        }
      }
      .err(e) => fail("afkd_board.cards: #{e.message}")
    }
  }
}
```
