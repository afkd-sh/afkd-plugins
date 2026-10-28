---
name: linear
description: Navigate and work the Linear issue this run works on — read and post comments, rewrite the issue's title and description, move it to another workflow state, work its sub-issues, browse the team's states and issues, handle the files it links, and file a follow-up issue. List the comments already on the issue; list its sub-issues and move one you have genuinely finished to a done state; browse states and issues to find related work; fetch a file the issue links (that is where a pasted screenshot lives) so you can look at it; post a comment to report a finished plan, a blocker, or a changed approach; ask a question that parks the issue until a human replies, when it does not say enough to build or its premise does not hold; retitle a groomed issue into house style and move it out of the state it was groomed in; label it; archive an issue that has resolved to nothing; put an artifact back on the issue; or file follow-up work you discovered as a new issue — where your run's instructions say, else in the team's backlog — instead of derailing onto it. Not for routine chatter.
allowed-tools: Bash(*/list_states.py *), Bash(*/list_states.py), Bash(*/list_issues.py *), Bash(*/list_comments.py *), Bash(*/list_comments.py), Bash(*/list_sub_issues.py *), Bash(*/list_sub_issues.py), Bash(*/move_sub_issue.py *), Bash(*/list_files.py *), Bash(*/list_files.py), Bash(*/fetch_file.py *), Bash(*/post_comment.py *), Bash(*/ask.py *), Bash(*/post_file.py *), Bash(*/create_issue.py *), Bash(*/read_description.py *), Bash(*/read_description.py), Bash(*/set_description.py *), Bash(*/set_title.py *), Bash(*/move_issue.py *), Bash(*/archive_issue.py *), Bash(*/archive_issue.py), Bash(*/list_labels.py *), Bash(*/list_labels.py), Bash(*/add_label.py *), Bash(*/remove_label.py *)
requires-executables: [python3]
---

## Read the active issue's comments

Run the bundled reader yourself through the Bash tool, with no argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_comments.py"`

It prints the newest comments oldest first, each as a line with its author and time
(` (edited)` when it was edited), then its text verbatim, with a blank line between
comments. An issue with no comments prints a single `no comments on issue …` line and
succeeds.

Optional flags:

- `--limit N` — how many of the newest comments to print (default 50).
- `--issue REF` — read a different issue, by its identifier (`ENG-12`) or its id
  (overrides `$LINEAR_ISSUE_ID`).

## Post a comment to the active issue

Run the bundled helper yourself through the Bash tool, **substituting your real
message** for the placeholder. Never run it with the placeholder text — the helper
rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/post_comment.py" "<REPLACE WITH YOUR COMMENT>"`

The comment is posted verbatim, as markdown. Pass `--issue REF` to post to a different
issue (overrides `$LINEAR_ISSUE_ID`). Anything long — a plan, a review, a patch — goes on
the issue as a file with `post_file.py` (see [Files](#files)), with a short comment saying
what it is.

## Ask for clarification and park the run

Two situations mean you should **stop and ask a human** rather than build:

- **A question.** The issue does not say enough to implement it confidently — the intent
  is unclear, a product decision is unmade, an acceptance criterion is missing — and
  reading the code and the issue's comments has not resolved it.
- **A verdict.** The issue's premise does not hold against the repository: the work has
  already landed, the bug does not reproduce, the file or symbol it names does not exist,
  or two of its requirements contradict each other.

In either case state it plainly — the question, or what you found and why the issue
cannot be built as written — and run the helper with that as its single argument
(**substitute your real message** for the placeholder):

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/ask.py" "<REPLACE WITH YOUR QUESTIONS>"`

In one step it posts your message as a comment on the active issue **and** parks the run.
afkd then **parks the issue**: it adds the `Needs Info` label, releases its claim, and moves
nothing else — the issue keeps its state, its place and its unspent attempts.
Nothing re-picks it while it waits. When a human replies in the thread, afkd finds the
labelled issue on a later poll, re-claims it wherever it sits, and a fresh run starts with
the answer under **New comments** in its brief.

After a successful call, **stop** — do not start implementing; the work resumes in the
next run. Ask only about genuine ambiguities and genuine contradictions, not things the
code itself would answer. Pass `--issue REF` to ask on a different issue (overrides
`$LINEAR_ISSUE_ID`).

## Read and update the issue description

When you maintain your interpretation or plan inside the issue **description** — under an
`<!-- afkd -->` marker, leaving the human's original ask untouched — read the current
description, edit only your marker region, then write the **whole** description back.
First read it:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/read_description.py"`

It prints the issue's current description verbatim (an empty description prints an empty
line and succeeds). Splice your `<!-- afkd -->` block into that text — preserving
everything else — write the result into `$AFKD_SCRATCH_DIR`, then set it back:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/set_description.py" --desc-file "$AFKD_SCRATCH_DIR/desc.md"`

`set_description.py` **replaces** the whole description, so always write back the full
text you read (original ask plus your edited region), never just your region. Its flags:

- `--desc "text"` — the new description inline. Mutually exclusive with `--desc-file`.
- `--desc-file PATH` — read the new description from a file (use this for a real,
  multi-line body). One of `--desc` / `--desc-file` is **required**.
- `--issue REF` — read/write a different issue (overrides `$LINEAR_ISSUE_ID`).

**Blast radius:** the credential is workspace-wide — an API key acts as the user who made
it, an app as itself — so an `--issue` override can rewrite the description of **any**
issue it reaches, and `set_description.py` overwrites the whole field: read first, splice,
write back. The default (no flag) stays scoped to this run's issue.

## Retitle the issue

When a groomed issue's title no longer says what the issue is, rewrite it —
**substituting your real title** for the placeholder. Never run it with the placeholder
text; the helper rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/set_title.py" "<REPLACE WITH YOUR NEW TITLE>"`

`set_title.py` **replaces** the whole title, so pass the finished title you want the issue
to carry. Pass `--issue REF` to retitle a different issue (overrides `$LINEAR_ISSUE_ID`).

**Blast radius:** the credential is workspace-wide, so an `--issue` override can retitle
**any** issue it reaches, and the old title is gone. The default (no flag) stays scoped to
this run's issue.

## Move the issue to another state

When you are done with the issue in the workflow state it sits in — a groomed issue
leaving triage, say — move it. Name the state by its exact name, or by its id from
`list_states.py` (see "Navigate the team"):

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/move_issue.py" "<STATE>"`

A name that matches no state of the team errors non-zero, lists the states there are, and
moves nothing. Optional flags:

- `--pos top|bottom` — where in the state's manual order the issue lands (default
  `bottom`, the end afkd puts back an issue it gave up at; afkd claims from the top).
- `--above ISSUE` / `--below ISSUE` — put the issue directly above or below ISSUE, an
  issue already in that state, named by its identifier or its id. It lands between ISSUE
  and ISSUE's neighbour (at the very top or bottom when ISSUE is at that end). An ISSUE
  that is not in the state exits non-zero and moves nothing, and an ISSUE that is the
  issue being moved is refused. Exclusive with `--pos` and with each other.
- `--issue REF` — move a different issue (overrides `$LINEAR_ISSUE_ID`).
- `--team ID` — read the states of a different team, named by its id, not its key
  (overrides `$LINEAR_TEAM_ID`).

**Blast radius:** the credential is workspace-wide, so an `--issue` override can move
**any** issue it reaches. Sharper still: moving an issue **into** a state the afkd linear
service picks from (`pick_from`) **enqueues autonomous work for afkd itself**, exactly as
filing one there does. Do it only when your run's instructions say to, and then only with
an issue that is a finished spec — one another agent can build without asking a question.
The helper never learns `pick_from` and cannot enforce this — you are the guardrail.

## Archive the issue

When an issue has resolved to nothing — a bug that did not reproduce, a duplicate, work
already done — and the owner has declared it a non-bug, archive it so it leaves every
view. It takes no argument; the issue is this run's own:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/archive_issue.py"`

To restore an issue you (or someone) archived, unarchive it — you must already know its
identifier or id, since an archived issue no longer appears in any read this skill makes:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/archive_issue.py" --unarchive --issue "<ISSUE>"`

Pass `--issue REF` to archive a different issue (overrides `$LINEAR_ISSUE_ID`).

**Blast radius:** the credential is workspace-wide, so an `--issue` override can archive
**any** issue it reaches. An archived issue leaves **every** state's listing — nothing in
this skill can list or find it again; the only way back is `--unarchive` with the
identifier you already know. The default (no flag) stays scoped to this run's issue.

## File a new issue

When you discover follow-up work while running this issue, **file it instead of doing
it**: a new issue keeps you on the task you were given. Substitute your real title for the
placeholder — the helper rejects it and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/create_issue.py" "<REPLACE WITH YOUR ISSUE TITLE>" --state "<STATE>"`

It files the issue in this run's team. Optional flags:

- `--desc "text"` — the issue's description.
- `--desc-file PATH` — read the description from a file instead. Use this for a real,
  multi-line body; write it into `$AFKD_SCRATCH_DIR` first. It is mutually exclusive
  with `--desc`.
- `--state STATE` — the workflow state it starts in, by exact name or id. Without it,
  Linear files it in the team's default state.
- `--pos top|bottom` — where in `--state` it lands (default `bottom`).
- `--above ISSUE` / `--below ISSUE` — file it directly above or below ISSUE, an issue
  already in `--state`, named by its identifier or id. It lands between ISSUE and ISSUE's
  neighbour (at the very top or bottom when ISSUE is at that end). An ISSUE that is not in
  the state exits non-zero and files nothing. Exclusive with `--pos` and with each other;
  all three need `--state`.
- `--parent ISSUE` — file it as a sub-issue of ISSUE (see [Sub-issues](#sub-issues)).
- `--team ID` — file it in a different team, named by its id, not its key (overrides
  `$LINEAR_TEAM_ID`).

On success it prints the new issue's identifier, its title and its url.

**Blast radius:** the credential is workspace-wide, so `--team` can file into **any** team
it reaches. Sharper still: filing an issue into a state the afkd linear service picks from
(`pick_from`) **enqueues autonomous work for afkd itself**. File a follow-up into the state
and at the place your run's instructions name. Only when they name none, file it into the
team's backlog, where a human promotes it. Filing into the build queue is right only when
the instructions say so, and then the issue must be a finished spec — one another agent can
build without asking a question. The helper never learns `pick_from` and cannot enforce
this — you are the guardrail.

## Sub-issues

An issue's sub-issues are its parts — where a Trello card has a checklist, a Linear issue
has sub-issues — and the brief already lists them, each with its state, under the issue.
To re-read them on demand:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_sub_issues.py"`

It prints one `- ENG-12 (In Progress) title` line per sub-issue, in Linear's order. An
issue with none prints a single `no sub-issues on issue …` line and succeeds.

When you have **genuinely completed** a sub-issue, move it to a done state, naming it by
its identifier and the state by its exact name or id:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/move_sub_issue.py" "<SUB-ISSUE>" "<STATE>"`

Move only what you truly finished. An issue that is not a sub-issue of this one is
refused and nothing moves. To split work into a new sub-issue, file it with
`create_issue.py --parent` (see [File a new issue](#file-a-new-issue)).

Both take `--issue REF` to work a different issue's sub-issues (overrides
`$LINEAR_ISSUE_ID`), and `move_sub_issue.py` takes `--team ID` (the team's id, not its key)
too.
**Blast radius:** the credential is workspace-wide, so a `move_sub_issue.py --issue` can
move the sub-issues of **any** issue it reaches — use the override deliberately; the
default (no flag) stays scoped to this run's issue.

## Labels

An issue's labels are the flags on it (mark a groomed issue `ready`, say, or clear that
flag). A label belongs to the team or to the whole workspace, so start by listing the
labels an issue of the team can carry — this also flags the ones already on the active
issue:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_labels.py"`

It prints one `id  color  name` line per label, appending `[workspace]` to a workspace
label and `[on issue]` to the ones the active issue carries. A team with no labels prints a
single `no labels on team …` line and succeeds.

Add a label to the active issue by its exact name, or by its id from that output:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/add_label.py" "<LABEL>"`

Remove a label the same way:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/remove_label.py" "<LABEL>"`

A name that matches no label errors non-zero; so does one that two labels carry (a team
label and a workspace label of one name, say), naming each id — pass the id you mean.
Adding a label the issue already has, or removing one it does not, is a no-op success
(it never duplicates and never errors). Both take `--issue REF` and `--team ID` (the
team's id, not its key).

**Blast radius:** the credential is workspace-wide, so an `--issue` override can flag or
unflag **any** issue it reaches. The default (no flag) stays scoped to this run's issue.

## Navigate the team

To find work related to this issue, browse the team's workflow states, then a state's
issues. First list the states:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_states.py"`

It prints one `id  type  name` line per state, in the order the team's workflow shows
them; the type is Linear's category (`triage`, `backlog`, `unstarted`, `started`,
`completed`, `canceled`). The team defaults from the environment; pass `--team ID` (the
team's id, not its key) to browse a different team.

Then list the issues in one state, named by its exact name or id:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_issues.py" "<STATE>"`

It prints one `identifier  title` line per open issue, in the state's manual order — the
order afkd claims in. Pass `--limit N` to bound how many are printed (default 50).

## Files

A file on a Linear issue is a link in its markdown: an image embedded `![name](url)` or a
file linked `[name](url)`, at a url Linear hosts, in the description or in a comment. A
screenshot a human pasted lives there. List the issue's files:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/list_files.py"`

It prints one `N  name  url` line per file — the description's first, then the thread's
oldest first, each url once — and passes over a link to anywhere else. An issue with none
prints a single `no files on issue …` line and succeeds.

Then fetch one by its number from that listing, or by its url:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/fetch_file.py" "<N>"`

It downloads the file into `$AFKD_SCRATCH_DIR/attachments/`, named as the service's
`fetch_files` names it (`1-shot.png`), and prints the written path — open it with the
`Read` tool to view an image.

Or fetch by name — the newest file named exactly that, say an earlier attempt's plan —
and choose where it lands:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/fetch_file.py" --name "<FILE-NAME>" --to "<ABSOLUTE-PATH>"`

`--name` takes the place of the number (give one or the other). With no file of that name
it prints `no file named …`, writes nothing and succeeds. `--to` works with a number too.
A file over 10 MiB is refused.

To hand an artifact back — an annotated screenshot, a rendered diff, a report — put a
local file on the issue:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/post_file.py" "/path/to/file" --comment "<WHAT IT IS>"`

It uploads the file to Linear and posts a comment holding it: an image shown, any other
file linked, after the `--comment` text when you give one. Optional flags:

- `--name "text"` — the name it is shown under (defaults to the file's basename).
- `--comment "text"` — the comment's text, above the file.
- `--issue REF` — put it on a different issue (overrides `$LINEAR_ISSUE_ID`).

A file larger than 10 MiB fails cleanly before any upload.

## How locators and credentials are resolved

Every helper reads its credential and Linear's endpoint from the environment afkd already
merged into this run (`LINEAR_AUTHORIZATION`, `LINEAR_API_URL`); you do not pass them. The
locators are read **from the environment only** — the active issue from
`$LINEAR_ISSUE_ID` (its identifier is `$LINEAR_ISSUE`) and the team from
`$LINEAR_TEAM_ID` — each overridable with the `--issue` / `--team` flag. An issue is
named by its identifier (`ENG-12`) or its id, either one, wherever a helper takes one. A
team is named by its id, as `$LINEAR_TEAM_ID` holds it, never by its key (`ENG`): Linear
answers a key with `Argument Validation Error`. The helper paths are passed on the command line because `CLAUDE_PLUGIN_ROOT` is not exported
into Bash subprocesses.

`AFKD_SCRATCH_DIR` — the per-run scratch dir — is additionally required by
`fetch_file.py` without `--to` (the download destination) and by `ask.py` (where its park
marker, `park`, is written). Both fail loudly when it is unset.

**Blast radius of the overrides.** The credential is workspace-wide, so `--issue` and
`--team` can reach **any** team or issue it can — not just this run's. Use the overrides
deliberately; the default (no flag) stays scoped to this run's issue and team.

A run stages the skill `@afkd/linear/linear` flattened, as `afkd-linear-linear`.

If a helper exits non-zero it has failed — **surface that failure** (report it as a
blocker), do not retry it blindly. That holds for a rate limit too: the helper says how
long Linear asked it to wait.
