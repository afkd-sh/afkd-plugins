---
name: trello
description: Navigate and work the Trello card this run works on, and the board around it — read the card whole, its comments, checklists and attachments, and find cards on the board by list, label or member; post, edit and delete comments; rewrite the card's name and description, set its due date, move it to another list, label it, put members on it, archive it; work the card's checklists and tick an item you have genuinely completed; browse the board's lists, labels and members; fetch an attachment (that is where a pasted screenshot lives, not in the comment text) so you can look at it, or attach an artifact back onto the card; ask a question that parks the card until a human replies, when it does not say enough to build or its premise does not hold; or file follow-up work you discovered as a new card — where your run's instructions say, else in Backlog — instead of derailing onto it. Not for routine chatter.
allowed-tools: Bash(*/card.py *), Bash(*/card.py), Bash(*/cards.py *), Bash(*/cards.py), Bash(*/comments.py *), Bash(*/comments.py), Bash(*/checklists.py *), Bash(*/checklists.py), Bash(*/check_items.py *), Bash(*/check_items.py), Bash(*/attachments.py *), Bash(*/attachments.py), Bash(*/download_attachment.py *), Bash(*/me.py *), Bash(*/me.py), Bash(*/boards.py *), Bash(*/boards.py), Bash(*/lists.py *), Bash(*/lists.py), Bash(*/labels.py *), Bash(*/labels.py), Bash(*/members.py *), Bash(*/members.py), Bash(*/create_card.py *), Bash(*/update_card.py *), Bash(*/archive_card.py *), Bash(*/archive_card.py), Bash(*/unarchive_card.py *), Bash(*/unarchive_card.py), Bash(*/delete_card.py *), Bash(*/create_comment.py *), Bash(*/update_comment.py *), Bash(*/delete_comment.py *), Bash(*/create_checklist.py *), Bash(*/delete_checklist.py *), Bash(*/create_check_item.py *), Bash(*/update_check_item.py *), Bash(*/delete_check_item.py *), Bash(*/create_attachment.py *), Bash(*/delete_attachment.py *), Bash(*/add_label.py *), Bash(*/remove_label.py *), Bash(*/add_member.py *), Bash(*/add_member.py), Bash(*/remove_member.py *), Bash(*/remove_member.py), Bash(*/ask.py *)
requires-executables: [python3]
---

## How the scripts work

Each script is one of the `@afkd/trello` plugin's operations, under the operation's name, and
does what the operation does: `update_card.py` is `trello.update_card`. It takes the
operation's parameters — the required ones as arguments, in the operation's order, the others
as flags of their own names (`--due_complete true`) — and prints what the operation gives
back, as JSON:

- a card as a `trello.Card` handle: `{"id": "Qb7eLy2w", "key": …, "fields": {…}}`, its `id`
  the card's short link, its fields `number`, `name`, `url`, `description`, `board`, `list`,
  `labels`, `members`, `due`, `due_complete`, `archived`, `created_at` and `last_activity_at`
  (unix milliseconds);
- a comment, a checklist, a check item, an attachment, a board, a list, a label or a member as
  its handle, its `id` Trello's own (a board's its short link, a member's their username), and
  its own fields (a comment's `card`, `body`, `author`, `created_at`, `edited_at`);
- a list as a JSON array, and nothing as `null`.

A write prints what it made or changed as Trello then holds it, so you never need to read it
back. **This run's card** is `$TRELLO_CARD_ID` and its board `$TRELLO_BOARD_ID`: a script that
acts on a card takes it as `--card ID`, and one that reads a board as `--board ID`, each
defaulting to the run's, so you pass neither to work this card on this board. A list, a label
and a member are named by their name as Trello shows it, matched **exactly** (a member by
username, or `"me"` — whoever this run acts as), and a list or a label by its id as well. A name
the board lacks is refused naming the ones it has, and one two share is refused listing their
ids. A flag that takes a list is given once per item: `--labels ready --labels "Needs Info"`.

## Read the card, its comments, checklists and attachments

Read the card whole — its name, its description verbatim, its list, labels, members and due
date:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/card.py"`

Read its comments, oldest first, each one's `body` verbatim and its `author`:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/comments.py"`

`--limit N` bounds how many (the newest N; default 200). afkd's own comments, such as
`[afkd-claim]`, are among them. The card's checklists, each with its items' names, and then its
check items one by one, each with its id and whether it is `done`:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/checklists.py"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/check_items.py"`

Its attachments are read under [Attachments](#attachments). Each of these takes `--card ID` to
read a different card.

## Post a comment

Run it yourself, **substituting your real message** for the placeholder — the script refuses
the placeholder and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_comment.py" "<REPLACE WITH YOUR COMMENT>"`

The comment is posted verbatim, as markdown; `--body_file PATH` takes it from a file instead
(use it for a real, multi-line comment, written into `$AFKD_SCRATCH_DIR` first), and
`--card ID` posts to a different card. It prints the new comment. A comment holds at most
16 384 characters. Anything longer — a plan, a review, a patch — goes on the card as a file
with `create_attachment.py` (see [Attachments](#attachments)), with a short comment saying
what it is.

To correct a comment you wrote, give its id from `comments.py` and the whole new text; to take
it back, delete it. Trello lets a token change only its own comments:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_comment.py" "<COMMENT-ID>" "<REPLACE WITH YOUR COMMENT>"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/delete_comment.py" "<COMMENT-ID>"`

## Ask for clarification and park the run

Two situations mean you should **stop and ask a human** rather than build:

- **A question.** The card does not say enough to implement it confidently — the
  intent is unclear, a product decision is unmade, an acceptance criterion is
  missing — and reading the code and the card's comments has not resolved it.
- **A verdict.** The card's premise does not hold against the repository: the work
  has already landed, the bug does not reproduce, the file or symbol it names does
  not exist, or two of its requirements contradict each other.

In either case state it plainly — the question, or what you found and why the card
cannot be built as written — and run the helper with that as its single argument
(**substitute your real message** for the placeholder):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/ask.py" "<REPLACE WITH YOUR QUESTIONS>"`

In one step it posts your message as a comment on the active card **and** parks the
run. afkd then **parks the card**: it adds the `Awaiting Reply` label, releases its
claim, and moves nothing else — the card keeps its place, its list and its unspent
attempts. Nothing re-picks it while it waits. When a human replies in the thread,
afkd finds the badged card on a later poll, re-claims it wherever it sits, and a
fresh run starts with the answer under **New comments** in its brief.

After a successful call, **stop** — do not start implementing; the work resumes in
the next run. Ask only about genuine ambiguities and genuine contradictions, not
things the code itself would answer. Pass `--card ID` to ask on a different card
(overrides `$TRELLO_CARD_ID`). Unlike the other scripts it prints a line of text, not JSON.

## Update the card

To change the card's **description**, read the current one with `card.py`, change it, then
write the **whole** description back — from a file, for a real multi-line body, written into
`$AFKD_SCRATCH_DIR` first:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_card.py" --description_file "$AFKD_SCRATCH_DIR/desc.md"`

`update_card.py` **replaces** the whole description and the whole name — there is no append or
edit-in-place — so always write back the full text you read with your change made in it, never
just the part you changed. To rename a groomed card whose name no longer says what it is,
**substitute your real title** for the placeholder; the script refuses the placeholder and
exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_card.py" --name "<REPLACE WITH YOUR NEW TITLE>"`

Its flags, each leaving what it is not given as it is, all sent in one request (with no flag
it sends nothing, and prints the card):

- `--name TEXT` — the new name.
- `--description TEXT` — the new description inline; exclusive with `--description_file PATH`.
  A description holds at most 16 384 characters.
- `--due RFC3339` — when the card is due (`2026-12-24T10:00:00Z`); `--clear due` takes the
  due date away. `--due_complete true|false` marks it done or not.
- `--list NAME` and `--at top|bottom` — see [Move the card](#move-the-card-to-another-list).
- `--card ID` — change a different card (overrides `$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card` override can
rewrite **any** card the token reaches, and the old text is gone. The default (no flag) stays
scoped to this run's card.

## Move the card to another list

When you are done with the card in the column it lives in — a groomed card leaving grooming,
say — move it, by the list's name (matched exactly, on this card's own board) or its id from
`lists.py`:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_card.py" --list "Backlog"`

A list the board lacks is refused naming the lists it has, and one that two lists share is
refused listing their ids (use the id). `--at top|bottom` says where in the list the card
lands: a card moved to another list lands at the **top** unless `--at bottom` says otherwise,
matching where a service's own `trello.update_card(account, card, list=…)` puts it, and
`--at` alone places the card in the list it is in:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_card.py" --at bottom`

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card` override can move
**any** card the token reaches. Sharper still: moving a card **into** the list the afkd trello
trigger claims from (`claim_from`) **enqueues autonomous work for afkd itself**, exactly as
creating one there does. Do it only when your run's instructions say to, and then only with a
card that is a finished spec — one another agent can build without asking a question. The
script never learns `claim_from` and cannot enforce this — you are the guardrail.

## Archive the card

When a card has resolved to nothing — a bug that did not reproduce, a duplicate, work already
done — and the owner has declared it a non-bug, archive it so it leaves the board. It takes no
argument; the card is this run's own, and it prints the card with `archived` true:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/archive_card.py"`

To restore a card you (or someone) archived, unarchive it — you must already know its id,
since an archived card no longer appears in any list read:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/unarchive_card.py" --card "<CARD-ID>"`

To delete a card for good — its comments, checklists and attachments with it, with no way
back — run `delete_card.py`; prefer archiving, and delete only a card you filed yourself by
mistake:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/delete_card.py" --card "<CARD-ID>"`

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card` override can
archive or delete **any** card the token reaches. An archived card leaves **every** list view
— `cards.py` shows it only with `--include_archived`. The default (no flag) stays scoped to this
run's card.

## File a new card

When you discover follow-up work while running this card, **file it instead of doing it**: a
new card keeps you on the task you were given. Take the target list's name from `lists.py`
(see [Navigate the board](#navigate-the-board)), then, **substituting your real title** for the
placeholder — the script refuses it and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_card.py" "<LIST>" "<REPLACE WITH YOUR CARD TITLE>"`

Optional flags:

- `--description TEXT` — the card's description; exclusive with `--description_file PATH`,
  which reads it from a file. Use that for a real, multi-line card body; write it into
  `$AFKD_SCRATCH_DIR` first. At most 16 384 characters.
- `--at top|bottom` — where in the list the card lands (default `bottom`).
- `--labels NAME`, `--members USERNAME` — a label of the board, a member of the board (`me`
  for this run's); each repeatable. Each is resolved before the card is made, so one the board
  lacks is refused and **nothing is filed**; a label is never created.
- `--due RFC3339` — when it is due.
- `--board ID` — file on a different board (overrides `$TRELLO_BOARD_ID`).

It prints the card it made. The list is **required** — there is no default, because a list is
not tied to this run the way the active card is.

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a list can belong to
**any** board the token reaches. Sharper still: creating a card into the list the afkd trello
trigger claims from (`claim_from`) **enqueues autonomous work for afkd itself**. File a follow-up
into the list and at the place your run's instructions name. Only when they name none, file it
into `Backlog`, where a human promotes it. Filing into the build queue is right only when the
instructions say so, and then the card must be a finished spec — one another agent can build
without asking a question. The script never learns `claim_from` and cannot enforce this — you
are the guardrail.

## Checklists

A card's checklist is its **acceptance contract**, not a scratchpad — the brief already shows
each checklist's items and their ids under `## Checklists`. When you have **genuinely
completed** an item, tick it by its id (from `check_items.py`):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/update_check_item.py" "<CHECK-ITEM-ID>" --done`

Tick only what you truly finished; do not tick an item just to clear the box. `--undone` marks
an item incomplete again, `--name TEXT` renames it, and with neither flag it sends nothing.
Ticking an item that is ticked is not an error. It prints the item.

To make a checklist, give its name and its items, in order; it prints the checklist:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_checklist.py" "<NAME>" --items "<FIRST ITEM>" --items "<SECOND ITEM>"`

An item Trello refuses leaves the checklist as far as it got, and the failure says how far.
To add one item to a checklist, by the checklist's id from `checklists.py` (`--at top` puts it
first; it goes last by default), and then to delete an item or a whole checklist:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_check_item.py" "<CHECKLIST-ID>" "<ITEM>"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/delete_check_item.py" "<CHECK-ITEM-ID>"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/delete_checklist.py" "<CHECKLIST-ID>"`

The scripts that name an item or a checklist's card take `--card ID` to work a different card
(overrides `$TRELLO_CARD_ID`). **Blast radius:** the board token is account-wide (ADR-0046 §A),
so a `--card` override can tick, add or delete items on **any** card the token reaches — use it
deliberately; the default (no flag) stays scoped to this run's card.

## Labels

A card's labels are the board-level flags on it (e.g. mark a groomed card `ready`, or clear
that flag). They are defined at **board** level, so start by listing the board's labels — name,
colour and id of each:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/labels.py"`

Then put one on the active card, by its exact name or its id; it prints the card:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/add_label.py" "ready"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/remove_label.py" "ready"`

`add_label.py` **never creates a label**: a name the board lacks is refused, naming the labels
the board has. A name two labels share is refused listing their ids — use the id. Adding a
label the card already has, or removing one it does not have, is not an error: the script
prints the card and changes nothing. Both take `--card ID` to work a different card (overrides
`$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card` override can flag
or unflag **any** card the token reaches. The default (no flag) stays scoped to this run's card.

## Members

A card's members are who works it. To put a member on the active card, by username, and to
take one off; the member defaults to `me`, whoever this run acts as:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/add_member.py"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/remove_member.py" "<USERNAME>"`

A username the board lacks is refused, naming the board's members (`members.py` lists them).
Adding a member already on the card, or removing one who is not, is not an error: the script
prints the card and changes nothing. Both take `--card ID` to work a different card (overrides
`$TRELLO_CARD_ID`). **Blast radius:** a `--card` override can change the members of **any**
card the token reaches; the default stays scoped to this run's card.

## Navigate the board

To find work related to this card, browse the board — the boards the token reaches, who it acts
as, the board's lists, members and cards:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/boards.py"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/me.py"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/lists.py"`

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/members.py"`

`lists.py` prints the board's open lists in the board's order; it, `labels.py` and
`members.py` take `--board ID` to browse a different board (overrides `$TRELLO_BOARD_ID`).
Then find cards:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/cards.py" --list "<LIST>"`

It prints the board's cards that every filter given keeps, list by list in the board's order,
each list top to bottom. The filters, each a flag:

- `--list NAME` — in any of the lists named; `--labels NAME` keeps a card carrying **every**
  label given, `--without_labels NAME` one carrying none of them; `--members USERNAME` — on any
  of the members given (`me` included). Each is repeatable, and a name the board lacks is
  refused naming the ones it has.
- `--include_archived` — archived cards, and cards in archived lists, too.
- `--limit N` — 1 to 1000 (default 100).
- `--board ID` — a different board (overrides `$TRELLO_BOARD_ID`).

## Attachments

The files the card holds — its uploads, and the Trello files its description and comments link
— are already in `task/`, and `task/README.md` links each by its relative name. A Trello url
still in the brief is a file that could not be downloaded when the run was claimed: fetch it
with `download_attachment.py`, below.

A screenshot a human "pasted into a comment" is stored by Trello as a **card attachment**, not
as comment text — so `comments.py` will not show it. To see it, list the card's attachments,
oldest first, each one's `name`, `bytes`, `mime_type` and `url`, and whether it is a file Trello
holds (`is_upload`) or a link:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/attachments.py"`

Then save a file by its id to an absolute path — under `$AFKD_SCRATCH_DIR`, say — and open it
with the `Read` tool to view an image:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/download_attachment.py" "<ATTACHMENT-ID>" "<ABSOLUTE-PATH>"`

It prints `null`; a link is refused, since it holds no file, and so is a file over 10 MiB or a
relative path. To hand an artifact back — an annotated screenshot, a rendered diff, a report —
attach a local file to the card, by its absolute path:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_attachment.py" "/path/to/file"`

`--name TEXT` gives the attachment a display name (default: the file's own name). A file over
10 MiB is refused before any upload. It prints the attachment. To take one off the card:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/delete_attachment.py" "<ATTACHMENT-ID>"`

Each takes `--card ID` to work a different card (overrides `$TRELLO_CARD_ID`). An attachment
is an artifact, not conversation, so it never enters the comment-delta bookkeeping.

## How locators and credentials are resolved

Every script reads its board credentials from the environment afkd already merged into this
run (`TRELLO_API_KEY` / `TRELLO_TOKEN`); you do not pass them. The locators are read **from the
environment only** — the active card from `$TRELLO_CARD_ID` and the board from
`$TRELLO_BOARD_ID` — each overridable with the `--card` / `--board` flag. A script that needs
one that is unset exits 1 naming it. The script paths are passed on the command line because
`CLAUDE_PLUGIN_ROOT` is not exported into Bash subprocesses (ADR-0017).

`AFKD_SCRATCH_DIR` — the per-run scratch dir — is additionally required by `ask.py` (where its
park marker, `park`, is written); it fails loudly when it is unset.

**Blast radius of the overrides.** The board token is account-wide, so `--card` and `--board`
(and a list or a board named in an argument) can reach **any** board, list, or card the token
can — not just this run's. Use the overrides deliberately; the default (no flag) stays scoped
to this run's card and board.

The staged directory is the flattened `trello` (ADR-0021).

If a script exits non-zero it has failed — **surface that failure** (report it as a blocker),
do not retry it blindly.
