---
name: trello
description: Navigate and work the Trello card this run works on — read and post comments, rewrite the card's title and description, move it to another list, work the card's checklists, browse the board's lists and cards, handle attachments, and file a follow-up card. List the comments already on the card; re-read the card's checklists and tick an item you have genuinely completed; browse lists and cards to find related work; fetch an attachment (that is where a pasted screenshot lives, not in the comment text) so you can look at it; post a comment to report a finished plan, a blocker, or a changed approach; ask a question that parks the card until a human replies, when it does not say enough to build or its premise does not hold; rename a groomed card into house style and move it out of the column it was groomed in; archive a card that has resolved to nothing so it leaves the board; attach an artifact back onto the card; or file follow-up work you discovered as a new card in the backlog instead of derailing onto it. Not for routine chatter.
allowed-tools: Bash(*/list_lists.py *), Bash(*/list_lists.py), Bash(*/list_cards.py *), Bash(*/list_comments.py *), Bash(*/list_comments.py), Bash(*/list_checklists.py *), Bash(*/list_checklists.py), Bash(*/check_item.py *), Bash(*/list_attachments.py *), Bash(*/list_attachments.py), Bash(*/fetch_attachment.py *), Bash(*/post_comment.py *), Bash(*/ask.py *), Bash(*/post_attachment.py *), Bash(*/create_card.py *), Bash(*/read_description.py *), Bash(*/read_description.py), Bash(*/set_description.py *), Bash(*/set_name.py *), Bash(*/move_card.py *), Bash(*/archive_card.py *), Bash(*/archive_card.py), Bash(*/list_labels.py *), Bash(*/list_labels.py), Bash(*/add_label.py *), Bash(*/remove_label.py *)
requires-executables: [python3]
---

## Read the active card's comments

Run the bundled reader yourself through the Bash tool, with no argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_comments.py"`

It prints each comment's author, time, and text. A card with no comments prints a
single `no comments on card …` line and succeeds.

Optional flags:

- `--limit N` — how many comments to fetch (default 50).
- `--card ID` — read a different card (overrides `$TRELLO_CARD_ID`).

## Post a comment to the active card

Run the bundled helper yourself through the Bash tool, **substituting your real
message** for the placeholder. Never run it with the placeholder text — the helper
rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/post_comment.py" "<REPLACE WITH YOUR COMMENT>"`

Pass `--card ID` to post to a different card (overrides `$TRELLO_CARD_ID`).

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
(overrides `$TRELLO_CARD_ID`).

## Read and update the card description

When you maintain your interpretation or plan inside the card **description** —
under an `<!-- afkd -->` marker, leaving the human's original ask untouched — read
the current description, edit only your marker region, then write the **whole**
description back. First read it:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/read_description.py"`

It prints the card's current `desc` verbatim (an empty description prints an empty
line and succeeds). Splice your `<!-- afkd -->` block into that text — preserving
everything else — write the result into `$AFKD_SCRATCH_DIR`, then set it back:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/set_description.py" --desc-file "$AFKD_SCRATCH_DIR/desc.md"`

`set_description.py` **replaces** the whole description, so always write back the
full text you read (original ask plus your edited region), never just your region.
Its flags:

- `--desc "text"` — the new description inline. Mutually exclusive with `--desc-file`.
- `--desc-file PATH` — read the new description from a file (use this for a real,
  multi-line body). One of `--desc` / `--desc-file` is **required**.
- `--card ID` — read/write a different card (overrides `$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card`
override can rewrite the description of **any** card the token reaches, and
`set_description.py` overwrites the whole field — read first, splice, write back.
The default (no flag) stays scoped to this run's card.

## Rename the card

When a groomed card's title no longer says what the card is, rewrite it —
**substituting your real title** for the placeholder. Never run it with the
placeholder text; the helper rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/set_name.py" "<REPLACE WITH YOUR NEW TITLE>"`

`set_name.py` **replaces** the whole title — there is no append or edit-in-place —
so pass the finished title you want the card to carry. Pass `--card ID` to rename a
different card (overrides `$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card`
override can rename **any** card the token reaches, and the old title is gone. The
default (no flag) stays scoped to this run's card.

## Move the card to another list

When you are done with the card in the column it lives in — a groomed card leaving
grooming, say — move it. Get the target list's id from `list_lists.py` (see
"Navigate the board"):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/move_card.py" "<LIST-ID>"`

or address the list by name instead of id (resolved against this card's own board):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/move_card.py" --name "Backlog"`

Take exactly one of a list id or `--name`; a `--name` that matches no list on the
board — or more than one, which Trello permits — errors non-zero and moves nothing
(use the id). Optional flags:

- `--pos top|bottom` — where in the target list the card lands (default `top`,
  matching where a service's own `move_to` puts a card).
- `--card ID` — move a different card (overrides `$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card`
override can move **any** card the token reaches, and a list id can address **any**
list on **any** board the token can. Sharper still: moving a card **into** the list
the afkd trello trigger picks from (`pick_from`) **enqueues autonomous work for afkd
itself**, exactly as creating one there does. The helper never learns `pick_from`
and cannot enforce this — you are the guardrail.

## Archive the card

When a card has resolved to nothing — a bug that did not reproduce, a duplicate, work
already done — and the owner has declared it a non-bug, archive it so it leaves the
board. It takes no argument; the card is this run's own:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/archive_card.py"`

To restore a card you (or someone) archived, unarchive it — you must already know its
id, since an archived card no longer appears in any list read:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/archive_card.py" --unarchive --card "<CARD-ID>"`

Pass `--card ID` to archive a different card (overrides `$TRELLO_CARD_ID`). Re-archiving
an already-archived card is a no-op success (it never errors and issues no extra read).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card` override
can archive **any** card the token reaches. An archived card leaves **every** list
view — nothing in this skill can list or find it again (a list read omits archived
cards); the only way back is `--unarchive` with the id you already know. The default (no
flag) stays scoped to this run's card.

## File a new card

When you discover follow-up work while running this card, **file it instead of
doing it**: a new card keeps you on the task you were given. Get the target list's
id from `list_lists.py` (see "Navigate the board"), then, **substituting your real
title** for the placeholder — the helper rejects it and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/create_card.py" "<LIST-ID>" "<REPLACE WITH YOUR CARD TITLE>"`

Optional flags:

- `--desc "text"` — the card's description.
- `--desc-file PATH` — read the description from a file instead. Use this for a
  real, multi-line card body; write it into `$AFKD_SCRATCH_DIR` first. It is
  mutually exclusive with `--desc`.
- `--pos top|bottom` — where in the list the card lands (default `bottom`: a filed
  follow-up joins the end of a list, it does not jump the queue).

On success it prints the created card's short url. The list id is **required** —
there is no default, because a list is not tied to this run the way the active card
is.

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a list id can
address **any** list on **any** board the token reaches. Sharper still: creating a
card into the list the afkd trello trigger picks from (`pick_from`) **enqueues
autonomous work for afkd itself**. File follow-ups into `Backlog`, where a human
promotes them. The helper never learns `pick_from` and cannot enforce this — you are
the guardrail.

## Checklists

A card's checklist is its **acceptance contract**, not a scratchpad — the brief
already shows each checklist's items and their checkItem ids under `## Checklists`.
When you have **genuinely completed** an item, tick it by its id:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/check_item.py" "<CHECK-ITEM-ID>"`

Tick only what you truly finished; do not tick an item just to clear the box. Pass
`--uncheck` to mark an item incomplete again. Re-running the same tick is
idempotent (setting an already-set state is not an error).

To re-read the current checklist state on demand (the brief is the primary read
path), list the card's checklists:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_checklists.py"`

It prints each checklist grouped, one line per item as `<checkItemId>  [x]  <name>`
(`[ ]` when incomplete). A card with no checklists prints a single
`no checklists on card …` line and succeeds.

Both take `--card ID` to work a different card (overrides `$TRELLO_CARD_ID`).
**Blast radius:** the board token is account-wide (ADR-0046 §A), so a
`check_item.py --card` can tick items on **any** card the token reaches — use the
override deliberately; the default (no flag) stays scoped to this run's card.

## Labels

A card's labels are the board-level flags on it (e.g. mark a groomed card `ready`,
or clear that flag). Labels are defined at **board** level and add/remove operate
on label **ids**, so start by listing the board's labels — this also flags the ones
already on the active card:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_labels.py"`

It prints one `id  color  name` line per board label, appending `[on card]` to the
ones the active card carries. A board with no labels prints a single
`no labels defined on board …` line and succeeds. Pass `--card ID` to resolve and
flag against a different card.

With an id from that output, add a label to the active card:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/add_label.py" "<LABEL-ID>"`

or address it by name instead of id (resolved against the board's labels):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/add_label.py" --name "ready"`

Remove a label the same way, by id or `--name`:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/remove_label.py" "<LABEL-ID>"`

Both `add_label.py` and `remove_label.py` take exactly one of a label id or
`--name`; a `--name` that matches no board label — or more than one — errors
non-zero (pass `--color` to disambiguate a repeated name, or use the id). Adding a
label the card already has, or removing one it does not, is a no-op success (it
never duplicates and never errors). Both take `--card ID` to work a different card
(overrides `$TRELLO_CARD_ID`).

**Blast radius:** the board token is account-wide (ADR-0046 §A), so a `--card`
override can flag or unflag **any** card the token reaches. The default (no flag)
stays scoped to this run's card.

## Navigate the board

To find work related to this card, browse the board's lists then a list's cards.
First list the board's lists (columns):

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_lists.py"`

It prints one `id  name` line per list. The board defaults from the environment;
pass `--board ID` to browse a different board.

Then, with a list id from that output, list the cards in it:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_cards.py" "<LIST-ID>"`

It prints one `id  name` line per card. The list id is **required** — there is no
default, because a list is not tied to this run the way the active card is. Pass
`--limit N` to bound how many cards are returned (default 50).

## Attachments

A screenshot a human "pasted into a comment" is stored by Trello as a **card
attachment**, not as comment text — so `list_comments.py` will not show it. To
see it, list the card's attachments, fetch the one you want, and `Read` the
downloaded file:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/list_attachments.py"`

It prints one `id  name  mimeType  bytes` line per attachment. A card with none
prints a single `no attachments on card …` line and succeeds. Pass `--card ID` to
inspect a different card.

Then fetch a single attachment by its id:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/fetch_attachment.py" "<ATTACHMENT-ID>"`

It downloads that attachment into `$AFKD_SCRATCH_DIR/attachments/` and prints the
written path — open it with the `Read` tool to view an image. Pass `--card ID` to
fetch from a different card.

To hand an artifact back — an annotated screenshot, a rendered diff, a report —
attach a local file to the card:

!`"${CLAUDE_PLUGIN_ROOT}/skills/trello/post_attachment.py" "/path/to/file"`

Optional flags:

- `--name "text"` — a display name for the attachment (defaults to the file's
  basename).
- `--card ID` — attach to a different card (overrides `$TRELLO_CARD_ID`).

A file larger than the ~10 MB Trello attachment cap fails cleanly before any
upload. An attachment is an artifact, not conversation, so it never enters the
comment-delta bookkeeping.

## How locators and credentials are resolved

Every helper reads its board credentials from the environment afkd already merged
into this run (`TRELLO_API_KEY` / `TRELLO_TOKEN`); you do not pass them. The
locators are read **from the environment only** — the active card from
`$TRELLO_CARD_ID` and the board from `$TRELLO_BOARD_ID` — each overridable with the
`--card` / `--board` flag (there is no `$AFKD_SCRATCH_DIR/card/id` fallback). The
helper paths are passed on the command line because `CLAUDE_PLUGIN_ROOT` is not
exported into Bash subprocesses (ADR-0017).

`AFKD_SCRATCH_DIR` — the per-run scratch dir — is additionally required by
`fetch_attachment.py` (the download destination) and by `ask.py` (where its park
marker, `park`, is written). Both fail loudly when it is unset.

**Blast radius of the overrides.** The board token is account-wide, so `--card`
and `--board` (and `list_cards.py`'s list id) can reach **any** board, list, or
card the token can — not just this run's. Use the overrides deliberately; the
default (no flag) stays scoped to this run's card and board.

The staged directory is the flattened `trello` (ADR-0021).

If a helper exits non-zero it has failed — **surface that failure** (report it as a
blocker), do not retry it blindly.
