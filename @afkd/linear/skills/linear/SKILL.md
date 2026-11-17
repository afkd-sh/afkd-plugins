---
name: linear
description: Work the Linear issue this run works on, and the workspace around it — read an issue whole, find issues by any filter, read an issue's comments and the files it links, browse a team's states and labels; change an issue's title, description, state, place, priority, estimate, due date, assignee, delegate, project, milestone, cycle or parent; label it, relate it to another issue, archive it; comment on it with files, or reply in a thread; ask a question that parks the issue until a human replies, when it does not say enough to build or its premise does not hold; or file follow-up work you discovered as a new issue — where your run's instructions say, else in the team's backlog — instead of derailing onto it. Not for routine chatter.
allowed-tools: Bash(*/issue.py *), Bash(*/issues.py *), Bash(*/comments.py *), Bash(*/states.py *), Bash(*/labels.py *), Bash(*/labels.py), Bash(*/files.py *), Bash(*/download_file.py *), Bash(*/create_issue.py *), Bash(*/update_issue.py *), Bash(*/add_label.py *), Bash(*/remove_label.py *), Bash(*/add_relation.py *), Bash(*/archive_issue.py *), Bash(*/create_comment.py *), Bash(*/ask.py *)
requires-executables: [python3]
---

## How the scripts work

Each script is one of the `@afkd/linear` plugin's actions, under the action's name, and does
what the action does: `update_issue.py` is `linear.update_issue`. It takes the action's
parameters — the required ones as arguments in the action's order, the others as flags of
their own names (`--due 2026-12-24`) — and prints what the action gives back, as JSON:

- an issue as a `linear.Issue` handle: `{"id": "ENG-123", "key": …, "fields": {…}}`, its
  fields its `uuid`, `team`, `title`, `url`, `description`, `state`, `state_type`,
  `priority`, `estimate`, `due`, `assignee`, `delegate`, `creator`, `project`, `milestone`,
  `cycle`, `labels`, `parent`, `sub_issues`, `blocks`, `blocked_by`, `related`, `similar`,
  `duplicates`, `duplicate_of`, `links`, `subscribers`, `created_at`, `updated_at`,
  `completed_at`, `canceled_at` (unix milliseconds), `archived` and `trashed`;
- a comment, a state or a label as its handle, its `id` its uuid;
- a list as a JSON array, and nothing as `null`.

A write prints what it made or changed as Linear then holds it, so you never need to read it
back. An **issue** is named by its identifier (`ENG-123`) or its uuid, either one. **This
run's issue** is `$LINEAR_ISSUE` (its uuid is `$LINEAR_ISSUE_ID`), so pass `"$LINEAR_ISSUE"`
wherever a script takes the issue. A team is named by its key (`ENG`), and a state, a label,
a project or a milestone by its name as Linear shows it, matched exactly, or by its id. A
user is named by display name or email, or `"me"` — whoever this run acts as. A flag that
takes a list is given once per item: `--labels Bug --labels "Needs Info"`.

## Read an issue

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/issue.py" "$LINEAR_ISSUE"`

It prints the issue whole: every field above, each list to its end. By its uuid an issue is
read anew even after it moved team and took another identifier. An issue Linear does not
hold fails naming it.

## Read the comments

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/comments.py" "$LINEAR_ISSUE"`

It prints the issue's comments, oldest first, each reply right after the comment it
answers: each one's `body` verbatim, its `author`, `created_at`, `edited_at`, `resolved_at`,
and `reply_to`, the id of the comment it answers. afkd's own bookkeeping comments, opening
`[afkd-claim]`, `[afkd-ran]`, `[afkd-attempt]` or `[afkd-park]`, are among them.

## Find issues

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/issues.py" --team "<TEAM>" --state "<STATE>"`

It prints the issues every filter given keeps, workspace-wide unless `--team` says, each read
whole. The filters, each a flag:

- `--id`, `--team`, `--state`, `--project`, `--milestone`, `--cycle N` — any of the values
  given; `--parent ISSUE` keeps its sub-issues.
- `--state_type` — `triage`, `backlog`, `unstarted`, `started`, `completed` or `canceled`.
- `--priority` — `none`, `urgent`, `high`, `medium` or `low`.
- `--labels` keeps an issue carrying **every** label given; `--without_labels` one carrying
  none of them.
- `--assignee`, `--delegate` — a display name or email, `me`, or `none` for nobody.
- `--blocked true` keeps an issue an open issue blocks, `false` one no open issue blocks.
- `--text` — in the title or the description, case ignored.
- `--created_after`, `--created_before`, `--updated_after`, `--updated_before`,
  `--completed_after`, `--completed_before`, `--canceled_after`, `--canceled_before` — a time
  as RFC 3339 (`2026-11-17T09:30:00Z`).
- `--due_after`, `--due_before` — a date, `YYYY-MM-DD`.
- `--include_archived` — archived and trashed issues too.
- `--order` — up to three of `manual`, `team`, `state`, `priority`, `assignee`, `project`,
  `cycle`, `due`, `estimate`, `title`, `created`, `updated`, `completed`, each breaking the
  ties of the one before (default: team, then state, then `manual`, top to bottom as Linear
  shows a state — the order afkd claims in).
- `--limit N` — 1 to 250 (default 50).

## Browse a team

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/states.py" "<TEAM>"`

It prints the team's workflow states in the order its workflow shows them, each one's `name`
and `type`. Then the labels an issue of the team can carry, the workspace's and the team's,
a workspace label's `team` null:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/labels.py" --team "<TEAM>"`

Without `--team` it prints the workspace's labels alone.

## Files

A file on a Linear issue is a link in its markdown — an image embedded `![name](url)` or a
file linked `[name](url)` — at a url Linear hosts, in the description or in a comment. A
screenshot a human pasted lives there. List the urls of the issue's files, the
description's first, then the comments' oldest first, each once:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/files.py" "$LINEAR_ISSUE"`

Then save one, by its url, to an absolute path — under `$AFKD_SCRATCH_DIR`, say — and open
it with the `Read` tool to look at an image:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/download_file.py" "<URL>" "<ABSOLUTE-PATH>"`

It fetches only from Linear, and refuses a file over 10 MiB. To put a file of your own on
the issue, post it with a comment (`create_comment.py --files`, below) or the description
(`update_issue.py --files`).

## Post a comment

Run it yourself, **substituting your real message** for the placeholder — the script
refuses the placeholder and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/create_comment.py" "$LINEAR_ISSUE" "<REPLACE WITH YOUR COMMENT>"`

The comment is posted verbatim, as markdown. Its flags:

- `--body_file PATH` — the comment from a file, in place of the argument: use it for a real,
  multi-line comment, written into `$AFKD_SCRATCH_DIR` first.
- `--files PATH` — an absolute path, put up and embedded after the comment: an image shown,
  any other file linked. Anything long — a plan, a review, a patch — goes on the issue as a
  file, with a short comment saying what it is.
- `--reply_to COMMENT` — answer that comment, by its id from `comments.py`: the reply goes in
  its thread.

It prints the new comment.

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

## Change the issue

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/update_issue.py" "$LINEAR_ISSUE" --title "<REPLACE WITH YOUR NEW TITLE>"`

Each field given is set, and each one left out is left as it is:

- `--title TEXT` — the whole title (the script refuses the placeholder).
- `--description TEXT` or `--description_file PATH` — the **whole** description: read it
  first with `issue.py`, make your change in that text, keeping every part you did not mean
  to change, write the result into `$AFKD_SCRATCH_DIR`, and pass the file. `""` empties it.
- `--files PATH` — beside a description: an absolute path put up and embedded after it.
- `--state STATE` — a state of the issue's team; `--team KEY` beside a `--state` of that team
  moves the issue to another team.
- `--at top`, `--at bottom`, or `--after ISSUE` — where it goes in its state, the new one or
  the one it is in; afkd claims from the top. `--after` names an issue already in that state.
- `--priority`, `--estimate N`, `--due YYYY-MM-DD`, `--assignee`, `--delegate`, `--project`,
  `--milestone` (of the issue's project, or of `--project`), `--cycle N`, `--parent ISSUE`.
- `--clear NAME` — clear `estimate`, `due`, `assignee`, `delegate`, `project`, `milestone`,
  `cycle` or `parent`.

Every name is looked up, and every file read, before anything changes: a name that matches
nothing fails listing what there is, and one that matches several listing their ids — pass
the id you mean. It prints the issue as Linear then holds it.

**Blast radius:** the credential is workspace-wide — an API key acts as the user who made
it, an app as itself — so a script can change **any** issue it reaches, not only this run's.
Sharper still: moving an issue **into** a state an afkd linear service claims from
(`claim_from`) **enqueues autonomous work for afkd itself**. Do it only when your run's
instructions say to, and then only with an issue that is a finished spec — one another agent
can build without asking a question. The script never learns `claim_from` and cannot enforce
this — you are the guardrail.

## Labels

Put a label on an issue, the issue's team's label of that name, else the workspace's, by its
exact name or its id from `labels.py`:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/add_label.py" "$LINEAR_ISSUE" "<LABEL>"`

Take one off the same way:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/remove_label.py" "$LINEAR_ISSUE" "<LABEL>"`

Adding a label the issue already carries, or removing one it does not, changes nothing and
succeeds. A name neither the team nor the workspace has fails naming the labels there are.

## Relate issues

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/add_relation.py" "$LINEAR_ISSUE" blocked_by "<ISSUE>"`

The relation is the issue's field the other issue lands in: `blocks`, `blocked_by`,
`related`, `similar`, `duplicate_of` or `duplicates`, and the other issue gets the matching
one. afkd never claims an issue an open issue blocks. A relation the issue already holds is
left as it is.

## Archive an issue

When an issue has resolved to nothing — a bug that did not reproduce, a duplicate, work
already done — and the owner has declared it a non-bug, archive it, its sub-issues with it:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/archive_issue.py" "<ISSUE>"`

An archived issue leaves every read but `issues.py --include_archived`.

## File a new issue

When you discover follow-up work while running this issue, **file it instead of doing
it**: a new issue keeps you on the task you were given. Substitute your real title for the
placeholder — the script refuses it and exits non-zero:

!`"${CLAUDE_PLUGIN_ROOT}/skills/linear/create_issue.py" "<TEAM>" "<REPLACE WITH YOUR ISSUE TITLE>" --state "<STATE>"`

It files the issue in the team whose key it is given, in the team's default state unless
`--state` names one, with the same flags as `update_issue.py` — `--description_file` for a
real, multi-line body, `--at` or `--after` to place it, `--parent ISSUE` to file it as a
sub-issue — and `--labels`, once per label. It prints the new issue.

**Blast radius:** filing an issue into a state an afkd linear service claims from
(`claim_from`) **enqueues autonomous work for afkd itself**. File a follow-up into the state
and at the place your run's instructions name. Only when they name none, file it into the
team's backlog, where a human promotes it. Filing into the build queue is right only when
the instructions say so, and then the issue must be a finished spec — one another agent can
build without asking a question.

## How the scripts reach Linear

Every script reads its credential and Linear's endpoint from the environment afkd already
merged into this run (`LINEAR_AUTHORIZATION`, `LINEAR_API_URL`); you do not pass them. They
reach the workspace this run's issue is in, and no other. The script paths are passed on the
command line because `CLAUDE_PLUGIN_ROOT` is not exported into Bash subprocesses.

`ask.py` also reads `$LINEAR_ISSUE_ID`, the active issue, and `$AFKD_SCRATCH_DIR`, the
per-run scratch dir where its park marker, `park`, is written; it fails loudly when either is
unset.

A run stages the skill `@afkd/linear/linear` flattened, as `afkd-linear-linear`.

If a script exits non-zero it has failed — **surface that failure** (report it as a
blocker), do not retry it blindly. That holds for a rate limit too: the script says how long
Linear asked it to wait.
