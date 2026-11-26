---
name: afkd_board
description: Work the board card this run was started for, and the board around it — read the card whole, its comments and its checklist, and find cards by list, label or text; comment on it, rewrite its title and description, move it to another list, label it, tick the checklist items you have genuinely done, mark it done, block or unblock it, archive it; browse the board's lists and labels; or file follow-up work you discovered as a new card instead of derailing onto it. Not for routine chatter.
allowed-tools: Bash(python3 */card.py *), Bash(python3 */cards.py *), Bash(python3 */cards.py), Bash(python3 */comments.py *), Bash(python3 */checklist.py *), Bash(python3 */lists.py), Bash(python3 */labels.py), Bash(python3 */create_card.py *), Bash(python3 */update_card.py *), Bash(python3 */move_card.py *), Bash(python3 */add_label.py *), Bash(python3 */remove_label.py *), Bash(python3 */add_item.py *), Bash(python3 */check_item.py *), Bash(python3 */remove_item.py *), Bash(python3 */mark_done.py *), Bash(python3 */add_blocked_by.py *), Bash(python3 */remove_blocked_by.py *), Bash(python3 */create_comment.py *), Bash(python3 */archive_card.py *), Bash(python3 */unarchive_card.py *), Bash(python3 */delete_card.py *), Bash(python3 */create_list.py *), Bash(python3 */update_list.py *), Bash(python3 */delete_list.py *)
requires-executables: [python3]
---

## How the scripts work

Each script is one of the `@afkd/afkd_board` plugin's operations, under the operation's name, and
does what the operation does: `move_card.py` is `afkd_board.move_card`. Run it with `python3`,
as `python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/move_card.py" 17 "In review"`. It takes the
operation's parameters (the required ones as arguments, in the operation's order, the others as
flags of their own names, `--at top`) and prints what the operation gives back, as JSON:

- a card as an `afkd_board.Card` handle: `{"id": "17", "key": "afkd_board:<board uuid>/<card
  uuid>", "fields": {…}}`, its `id` the card's number as its digits, its fields `number`,
  `title`, `description`, `list`, `labels`, `archived`, `done`, `blocked_by`, `blocking`,
  `blocked`, `url`, `created_at` and `updated_at` (unix milliseconds);
- a comment as an `afkd_board.Comment` handle, its `id` the comment's uuid and its fields `card`,
  `body`, `author`, `run`, `created_at` and `url`;
- a list, a label or a checklist item as an object (`{"id", "name"}`, `{"name", "colour"}`,
  `{"text", "done"}`);
- a list of any of them as a JSON array, and nothing as `null`.

A write prints what it made or changed as the board then holds it, so you never need to read it
back. **This run's card** is the one in your brief (`task/README.md`), by its number: every script
that acts on a card takes it as its first argument, so `card.py 17` reads card #17. A list or a
label is named by its name as the board shows it, matched **exactly**. A list the board lacks is
refused naming the lists it has. A flag that takes a list is given once per item:
`--label ready --label "needs info"`.

The scripts reach the board through `$AFKD_BOARD_URL` and `$AFKD_BOARD_AUTHORIZATION`, which the
run carries; a script that needs one that is unset exits 1 naming it. A refusal prints the
plugin's sentence after the script's name on stderr and exits 1.

**The `afkd/claimed` label is the service's.** It is on this run's card because the service claimed
the card for you, and it comes off once the run has ended. Do not put it on or take it off.

**Under which run a change shows.** A change of a card, its checklist or a comment is shown on the
board under the run it was made in, when the script knows the run: its `machine`, `service`,
`instance` and `id`. The id is the leaf of `$AFKD_SCRATCH_DIR`, the instance `$AFKD_INSTANCE`
(else `0`) and the service the one `$AFKD_BOARD_RUN` carries; afkd does not yet give a run its
machine in a variable, so until it does no script sends a `run`: the board shows your changes
under the key's name, and the script says so once on stderr. Nothing else is different.

## Read the card, its comments and its checklist

Read the card whole: its title, its description verbatim, its list, labels and blocks.

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/card.py" 17

Read its comments, oldest first, each one's `body` verbatim and its `author`. afkd's own claim
markers (`[afkd-claim] owner=…`) are among them, made by a key.

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/comments.py" 17

Read its checklist, its items in the card's order:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/checklist.py" 17

## Find cards

`cards.py` keeps the cards every filter given keeps, in the board's order, and prints their
handles. A filter left out keeps every card:

| Flag | Keeps |
|------|-------|
| `--list NAME` | a card in this list; repeat it for several, a card in any |
| `--label NAME` | a card carrying this label; repeat it, a card carrying every one |
| `--without_label NAME` | a card carrying none of these labels; repeat it |
| `--done true\|false` | the cards that are done, or those that are not |
| `--blocked true\|false` | the blocked cards, or the free ones |
| `--text TEXT` | a card with it in its title or its description, any case |
| `--include_archived` | the archived cards too |
| `--limit N` | at most this many, 1 to 250 (default 50) |

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/cards.py" --list Todo --without_label hold --done false

The board's lists and labels:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/lists.py"
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/labels.py"

## Comment on the card

Run it yourself, **substituting your real message** for the placeholder: the script refuses the
placeholder and exits non-zero. It posts the comment verbatim, markdown, 1 to 10 000 characters,
and prints it as a comment handle.

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/create_comment.py" 17 "<REPLACE WITH YOUR COMMENT>"

`--body_file PATH` takes the comment from a file instead: use it for a real, multi-line comment,
written into `$AFKD_SCRATCH_DIR` first.

## Change the card

`update_card.py` **replaces** the whole title or the whole description, and there is no append: to
change the description, read the current one with `card.py`, change it, and write the **whole**
text back, from a file for a real multi-line body. Give `--title`, a description (`--description`
or `--description_file`), or both:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/update_card.py" 17 --description_file "$AFKD_SCRATCH_DIR/desc.md"

`move_card.py CARD LIST` moves the card to a list, at the bottom unless `--at top|bottom` or
`--after N` (the number of a card in that list) says:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/move_card.py" 17 "In review" --at top

`add_label.py CARD LABEL` puts a label on the card (one the board lacks is made in the next free
colour), and `remove_label.py CARD LABEL` takes one off; taking off a label the card lacks, or the
board does, changes nothing:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/add_label.py" 17 needs-review
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/remove_label.py" 17 needs-review

`mark_done.py CARD` marks the card done, and `--done false` not; a blocked card is refused naming
the cards that are not done. `add_blocked_by.py CARD OTHER` makes card OTHER block it (a block that
makes a chain back is refused naming the chain), and `remove_blocked_by.py CARD OTHER` takes the
block off:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/mark_done.py" 17
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/add_blocked_by.py" 17 12

`archive_card.py CARD` archives the card, which leaves its list; `unarchive_card.py CARD` brings it
back to the bottom of its list. `delete_card.py CARD` deletes it for good, with its checklist, its
comments and its activity, and prints the card as it was: do that only when your instructions say
so.

## Work the checklist

Tick an item you have **genuinely completed**, by its place in the card's checklist, counting from
0 (`checklist.py` gives the order). `--done false` unticks it:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/check_item.py" 17 0
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/check_item.py" 17 0 --done false

`add_item.py CARD TEXT` adds an unticked item to the end, and `remove_item.py CARD INDEX` removes
one, the later items moving up:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/add_item.py" 17 "Bound the backoff"
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/remove_item.py" 17 2

## File follow-up work

When you discover work that is not this card's, make a card for it instead of derailing onto it,
where your run's instructions say, else in the board's first list. `create_card.py LIST TITLE`
makes a card with the board's next number, at the bottom unless `--at` or `--after N` says, and
prints it. **Substitute your real title** for the placeholder. `--description` (or
`--description_file`, for a real multi-line body), `--label NAME` and `--item TEXT` (each given
once per item, the items not ticked) fill it in:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/create_card.py" "Todo" "<REPLACE WITH YOUR CARD TITLE>" --description_file "$AFKD_SCRATCH_DIR/follow-up.md" --label bug

## Lists

A list is named by its name, and a change of a list is shown under no run:

    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/create_list.py" "Review" --after "Todo"
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/update_list.py" "Review" --name "Reviewed" --at bottom
    python3 "${CLAUDE_PLUGIN_ROOT}/skills/afkd_board/delete_list.py" "Reviewed"

`create_list.py NAME` makes the list at the bottom of the board unless `--at top|bottom` or
`--after LIST` says, and prints it as `{"id", "name"}`. `update_list.py LIST` renames it with
`--name`, moves it with `--at` or `--after`, or both, and prints it. `delete_list.py LIST` deletes
it with its archived cards and prints `null`; a list holding cards that are not archived is
refused naming how many.

## Every script

| Script | Arguments | Prints |
|--------|-----------|--------|
| `card.py` | `CARD` | the card |
| `cards.py` | `[--list NAME]… [--label NAME]… [--without_label NAME]… [--done true\|false] [--blocked true\|false] [--text TEXT] [--include_archived] [--limit N]` | an array of cards |
| `comments.py` | `CARD` | an array of comments, oldest first |
| `checklist.py` | `CARD` | an array of `{"text", "done"}` |
| `lists.py` | | an array of `{"id", "name"}` |
| `labels.py` | | an array of `{"name", "colour"}` |
| `create_card.py` | `LIST TITLE [--description TEXT \| --description_file PATH] [--label NAME]… [--item TEXT]… [--at top\|bottom \| --after N]` | the card made |
| `update_card.py` | `CARD [--title TEXT] [--description TEXT \| --description_file PATH]` | the card |
| `move_card.py` | `CARD LIST [--at top\|bottom \| --after N]` | the card |
| `add_label.py`, `remove_label.py` | `CARD LABEL` | the card |
| `add_item.py` | `CARD TEXT` | the card |
| `check_item.py` | `CARD INDEX [--done true\|false]` | the card |
| `remove_item.py` | `CARD INDEX` | the card |
| `mark_done.py` | `CARD [--done true\|false]` | the card |
| `add_blocked_by.py`, `remove_blocked_by.py` | `CARD OTHER` | the card |
| `create_comment.py` | `CARD (BODY \| --body_file PATH)` | the comment made |
| `archive_card.py`, `unarchive_card.py` | `CARD` | the card |
| `delete_card.py` | `CARD` | the card as it was |
| `create_list.py` | `NAME [--at top\|bottom \| --after LIST]` | the list |
| `update_list.py` | `LIST [--name NAME] [--at top\|bottom \| --after LIST]` | the list |
| `delete_list.py` | `LIST` | `null` |
