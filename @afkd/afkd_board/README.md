# `@afkd/afkd_board`

A **provider** plugin that reads and writes a board on [app.afkd.sh](https://app.afkd.sh): one
card by its number, the cards any filters keep, a card's comments, and the board's lists and
labels, and [every write](#writing-to-a-board) of its cards, checklists, comments and lists, each
write of a card made under the run it is made in. Each call reaches the board through the
[`afkd_board.Board`](#afkd_boardboard) it is passed, by one of the board's API keys, so a config
works two boards side by side with two of them. It declares no trigger kind: every call a config
makes is a child afkd starts for that one call.

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
is: the manifest, the `afkd-afkd_board` binary and the `LICENSE-MIT` and `LICENSE-APACHE`
texts, nothing else. Nothing is compiled on your host, and no Rust toolchain is needed
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

A card's checklist is not among its fields: afkd refuses a handle's field typed as a struct,
and an item, an [`afkd_board.Item`](#the-values), is one.

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
| `afkd_board.Item`  | `text: str`, `done: bool`: an item of a card's checklist |

### The reads

| Call | Gives |
|------|-------|
| `afkd_board.card(b, number)` | `Card`, archived or not; a number the board has no card of is an `.err`, ``card: The board has no card #17.`` |
| `afkd_board.cards(b, ~list, ~labels, ~without_labels, ~done, ~blocked, ~text, ~include_archived, ~limit)` | `list[Card]` in the board's order, [filtered](#afkd_boardcardss-filters) |
| `afkd_board.comments(b, card)` | `list[Comment]`, oldest first |
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
- A card of another board than the `afkd_board.Board` reaches, passed to `comments`, naming both
  boards: ``comments: card is #17 of board <its board's uuid>, and the afkd_board.Board reaches board <the other's uuid>``.
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
is refused, naming both:

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
