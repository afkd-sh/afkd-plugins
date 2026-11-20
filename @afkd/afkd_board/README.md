# `@afkd/afkd_board`

A **provider** plugin that reads a board on [app.afkd.sh](https://app.afkd.sh): one card by
its number, the cards any filters keep, a card's comments and its checklist, and the board's
lists and labels.
Each read reaches the board through the [`afkd_board.Board`](#afkd_boardboard) it is passed, by
one of the board's API keys, so a config reads two boards side by side with two of them. It
declares no trigger kind: every call a config makes is a child afkd starts for that one call.

It reads the board's HTTP API, which the site's README documents (*A board's API*). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[the afk language](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/afkd_board
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest and the `afkd-afkd_board` binary, nothing else. Nothing is compiled on your
host, and no Rust toolchain is needed there. Installing it needs **afkd 0.2.679 or newer**,
the first whose manifests type a handle's field as a `time` and a parameter as optional;
`afkd update @afkd/afkd_board` moves an installed copy onto the next release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/afkd_board/Cargo.toml
$ tools/pack-release afkd_board /tmp/afkd-release @afkd/afkd_board/target/release/afkd-afkd_board
$ afkd install /tmp/afkd-release/afkd-afkd_board
```

The configs below are written in the afk language of **afkd 0.2.694 or newer**, the first
that reads `env.get(…) ?? fail(…)` in a constant at load; an older afkd still runs the plugin,
but refuses the examples when they load.

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
