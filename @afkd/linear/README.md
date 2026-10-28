# `@afkd/linear`

A **provider** plugin with one trigger kind, [`service(linear)`](#servicelinear-issues): it
fires when an issue lands in a watched workflow state of a Linear team, and gives the
service's slots the actions that move, label, delegate, comment on, describe and file issues as work
starts, finishes, or fails, that work an issue's sub-issues, and that put a file on an issue
and fetch its files back. It also ships the `linear` skill an agent works the issue through:
read and post comments, fetch and put up files, move the issue and its sub-issues, label
it, file follow-up issues, and ask a question that parks the issue until a human replies.

It does for a Linear team what [`@afkd/trello`](../trello) does for a board: the same
claim, watermark, attempt and park comments on the issue, the same brief and the same run
environment, mapped onto Linear's states, labels, assignees and sub-issues. The places it
behaves differently are listed [at the end](#where-it-differs-from-afkdtrello). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[config language v2](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/linear
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest, the `afkd-linear` binary and its `skills/`, nothing else. Nothing is
compiled on your host, and no Rust toolchain is needed there. Installing it needs **afkd
0.2.414 or newer**, the first that fills in the platform; `afkd update @afkd/linear` moves
an installed copy onto the next release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/linear/Cargo.toml
$ tools/pack-release linear /tmp/afkd-release @afkd/linear/target/release/afkd-linear
$ afkd install /tmp/afkd-release/afkd-linear
```

The configs below are written in the config language of **afkd 0.2.435 or newer**, the
first that reads a failure as `r.error != nil`; an older afkd still runs the plugin, but
refuses the examples when they load.

A config file uses the plugin by importing it, and then names it by its leaf, `linear`:

```conf
import "core"
import "core:env"
import "@afkd/linear"

LINEAR_API_KEY :: env.get("LINEAR_API_KEY") ?? ""

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task.md
}

develop :: service(linear) {
  team         "ENG"
  api_key      LINEAR_API_KEY
  pick_from    "Todo"
  max_attempts 2
  every        1m~3m

  on_claim proc(issue: linear.Issue) {
    moved := linear.move_to(issue, "In Progress")
    if moved != nil { fail("linear.move_to: #{moved.message}") }
  }
  on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {
    moved := linear.move_to(issue, "In Review")
    if moved != nil { fail("linear.move_to: #{moved.message}") }
    said := linear.comment(issue, "afkd landed this issue in #{outcome.duration}.")
    if said != nil { fail("linear.comment: #{said.message}") }
  }
  on_park proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {
    said := linear.comment(issue, "parked after #{outcome.duration}: waiting for a reply.")
    if said != nil { fail("linear.comment: #{said.message}") }
  }
  on_fail proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {
    moved := linear.move_to(issue, "Backlog")
    if moved != nil { fail("linear.move_to: #{moved.message}") }
    labelled := linear.add_label(issue, "Problem")
    if labelled != nil { fail("linear.add_label: #{labelled.message}") }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) { task() }
}
```

The credential comes from the daemon's environment, so it never sits in the file. Each
action names the issue it acts on — the `issue` its slot is passed — so it reads the same
wherever it is written.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
import "vendor:claude"

// The agent a `service(linear)` runs: the skill is what lets it work the issue.
builder :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/linear/linear" ]
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`LINEAR_API_URL`, `LINEAR_AUTHORIZATION`, `LINEAR_TEAM_ID`, `LINEAR_ISSUE_ID` and
`LINEAR_ISSUE` every `service(linear)` run carries: the GraphQL endpoint, the
`Authorization` header the run reaches Linear with, the team's id, and the active issue's id
and identifier (`ENG-123`). A script takes `--issue` and `--team` (the team's id, not its
key) to reach another issue or team. Each script is a `python3` program of the standard
library alone.

| Script                | What it does                                                        |
|-----------------------|---------------------------------------------------------------------|
| `list_comments.py`    | prints the issue's comments, oldest first                           |
| `post_comment.py`     | posts a comment                                                     |
| `ask.py`              | posts a question and parks the run                                  |
| `read_description.py` | prints the description                                              |
| `set_description.py`  | replaces the description                                            |
| `set_title.py`        | replaces the title                                                  |
| `move_issue.py`       | moves the issue to a state, at an end or beside another issue       |
| `archive_issue.py`    | archives the issue, or with `--unarchive` restores it               |
| `create_issue.py`     | files a new issue or sub-issue, in a state and at a place           |
| `list_sub_issues.py`  | prints the sub-issues with their states                             |
| `move_sub_issue.py`   | moves a sub-issue to a state                                        |
| `list_labels.py`      | prints the team's and the workspace's labels                        |
| `add_label.py`        | puts a label on the issue                                           |
| `remove_label.py`     | takes a label off the issue                                         |
| `list_states.py`      | prints the team's workflow states                                   |
| `list_issues.py`      | prints the issues in a state, in manual order                       |
| `list_files.py`       | prints the files the issue links                                    |
| `fetch_file.py`       | downloads one of them                                               |
| `post_file.py`        | puts a file on the issue in a comment                               |

The skill's `SKILL.md` says when to use each and what each override can reach.

## `service(linear)` (issues)

Fires when an issue lands in a watched workflow state; its slots move issues across the
team's workflow as work starts, finishes, or fails. Its secrets stay with the plugin and the
run.

| Key              | Type           | Notes                                  |
|------------------|----------------|----------------------------------------|
| `team`           | `string`       | the team's key, the prefix of every identifier it holds (`ENG`); required |
| `api_key`        | `string`       | a Linear API key; exactly one of it or the app's two keys below |
| `client_id`      | `string`       | an OAuth app's client id, with `client_secret` |
| `client_secret`  | `string`       | the OAuth app's client secret          |
| `base_url`       | `string`       | API root; defaults to `https://api.linear.app` (a test seam) |
| `pick_from`      | `list[string]` | the workflow states new issues are drawn from, by name, in the order written; required |
| `require_member` | `list[string]` | `"me"`, users by display name or email, or `"none"`: only claim issues assigned or delegated to one of them, `"none"` an issue nobody holds; default unset |
| `require_label`  | `string`       | only claim issues carrying a label of this exact name; default unset |
| `without_label`  | `list[string]` | never claim issues carrying any of these labels (deny wins over `require_label`); default unset |
| `discuss_with`   | `list[string]` | `[ "anyone" ]` or users, `[ "chen@acme.example", "me" ]`: claim an issue on first sight or when an allowed author comments after afkd last did; default unset |
| `max_attempts`   | `int`          | retries per issue; default `1`         |
| `every`          | `duration`     | how often the team is polled; default `30s`; a range `2m~3m` jitters |

The last two are afkd's own, read by afkd for every kind that claims its work.

The service's slots are code ([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/)), each a `proc`
passed the run (all but `on_claim`, which runs before one exists), the claimed issue, and
after the run its outcome — by position, and a slot writes every one of them, in order,
each with its type:

| Slot                                                                         | Runs                                            |
|------------------------------------------------------------------------------|-------------------------------------------------|
| `on_run proc(run: core.Run, issue: linear.Issue)`                            | the run itself                                  |
| `on_claim proc(issue: linear.Issue)`                                         | once the issue is claimed, before the run       |
| `on_done proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome)`    | after a run that finished                       |
| `on_park proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome)`    | after a run that ended waiting on a human reply |
| `on_fail proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome)`    | after a run that failed                         |

`run` is a `core.Run` (`id`, `scratch_dir`) and `outcome` a `core.Outcome` (`duration`,
`error`). `issue` is a **`linear.Issue`**, the plugin's handle for the issue:

| Field        | Type           | What it is                                                |
|--------------|----------------|-----------------------------------------------------------|
| `id`         | `string`       | the issue's identifier, `ENG-123`                         |
| `key`        | `string`       | afkd's claim key for the issue                            |
| `title`      | `string`       | the issue's title                                         |
| `url`        | `string`       | the issue's address on linear.app                         |
| `labels`     | `list[string]` | the names of the labels on the issue                      |
| `state`      | `string`       | the workflow state it sat in when it was claimed          |
| `parent`     | `string`       | the parent issue's identifier, `""` for no sub-issue      |
| `sub_issues` | `list[string]` | the sub-issues' identifiers, in Linear's order            |
| `blocked_by` | `list[string]` | every issue that blocks this one, done or not             |

A config passes an issue around and compares it, but never builds one. The plugin sends
every field with the claim, as the claiming poll read the issue, and they hold for the whole
run.

**Credentials.** A personal API key acts as the user who made it. An OAuth app's
`client_id` and `client_secret` are traded for the app's own token with Linear's
`client_credentials` grant, asking for `read,write,app:assignable,app:mentionable`, and the
token is fetched again before it expires: the app is then the actor on every comment, and
`"me"` names the app. Exactly one of the two is written.

```conf
import "core"
import "core:env"
import "@afkd/linear"

LINEAR_CLIENT_ID     :: env.get("LINEAR_CLIENT_ID") ?? ""
LINEAR_CLIENT_SECRET :: env.get("LINEAR_CLIENT_SECRET") ?? ""

triage :: service(linear) {
  team           "ENG"
  client_id      LINEAR_CLIENT_ID
  client_secret  LINEAR_CLIENT_SECRET
  pick_from      "Triage"
  require_member "me"

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Actions.** afkd runs a slot's statements in the order they are written, and each action
it calls is one request to the plugin, which does it on the issue the action is passed
first. A state or a label is named by its name, in the service's team:

| Action                                                       | Effect                                  |
|--------------------------------------------------------------|-----------------------------------------|
| `linear.comment(issue, "<text>", files=[…])`                 | post a comment, verbatim, the files embedded after it |
| `linear.describe(issue, "<text>", files=[…])`                | replace the description, the files embedded after it |
| `linear.rename(issue, "<title>")`                            | replace the title                       |
| `linear.move_to(issue, "<state>")`                           | move the issue to a workflow state; its place there is Linear's to pick |
| `linear.add_label(issue, "<label>")`                         | put on the team's label of that name, else the workspace's, else one created for the team |
| `linear.remove_label(issue, "<label>")`                      | take a label off; a name with no label is a no-op |
| `linear.delegate(issue, "<to>")`                             | delegate the issue to `"me"`, the app, or to an app's user by display name or email |
| `linear.create_issue(issue, "<title>", "<description>", state="<state>")` | file a new issue in the team, in the team's default state when `state` is left out |
| `linear.create_sub_issue(issue, "<title>", "<description>", state="<state>")` | file a new sub-issue of the issue, as `create_issue` does |
| `linear.archive(issue)`                                      | archive the issue                       |
| `linear.list_comments(issue, "<to>")`                        | write the thread, oldest first as the brief renders it, to `to` |
| `linear.read_description(issue, "<to>")`                     | write the description as Linear holds it now to `to` |
| `linear.list_sub_issues(issue, "<to>")`                      | write the sub-issues, a line each with its state, to `to` |
| `linear.move_sub_issue(issue, "<sub-issue>", "<state>")`     | move the sub-issue, by identifier, to a workflow state |
| `linear.fetch_files(issue, "<to>")`                          | write every file the issue links into the directory `to`, numbered |

The issue is a `linear.Issue` and nothing else: `linear.move_to("Done")`, with no issue, is
refused when the config loads, and so is an action the plugin does not declare. A read
cannot hand a value back, since a plugin exposes none, so it writes what it read to the
absolute path `to`, through a temp file renamed onto it.

Each action returns an error or `nil`, like any call: one Linear refused — a state the team
does not have, a sub-issue that is not the issue's — fails with the plugin's sentence. A
failing `on_claim` gives the issue back and fails the run; a failing post-run slot is logged
and changes nothing. afkd never retries a slot's action; the plugin retries what Linear
could not take.

**When Linear cannot be reached.** A write Linear could not be reached for, or answered
with a rate limit or a 5xx, is queued and the call answers as if it had landed, so the
slot's next action still runs and is queued behind it. The plugin retries the queue in the
background, each issue's writes in the order they were asked for: the first retry 5 seconds
after the failure, the gap doubling to 5 minutes and never shorter than a `Retry-After`
Linear named, for up to an hour; then it gives up with one loud line naming the issue and
the write it lost. A refusal no retry can fix is never queued: it is the call's error. The
queue is held in memory, so a plugin afkd ends names each write it still owes. A read is
never queued: when Linear cannot be reached it fails now, with the reason.

**Files on the issue.** A Linear file is a link in an issue's markdown: `comment` and
`describe` take `files=[…]`, absolute paths, and each file is read when the slot calls, put
up through Linear's `fileUpload`, and embedded after the text on a paragraph of its own — an
image shown `![name](url)`, any other file linked `[name](url)`. The upload never carries
the credential. A write queued behind an outage still puts up what the slot read, even once
the file has moved or gone, and a file already up is not put up again. `fetch_files` writes
every file Linear hosts that the issue links — its description's, then its thread's oldest
first, each url once — into the existing directory `to`, numbered in that order
(`1-screenshot.png`, `2-run.log`). It fetches them all before it writes any, with the
credential, from Linear's file host or the API root and from no other. Each file, either
way, is at most 10 MiB.

```conf
import "core"
import "@afkd/linear"

develop :: service(linear) {
  team      "ENG"
  api_key   "REPLACE_ME"
  pick_from "Todo"

  on_claim proc(issue: linear.Issue) {
    fetched := linear.fetch_files(issue, "/srv/acme/widgets/files")
    if fetched != nil { fail("linear.fetch_files: #{fetched.message}") }
  }
  on_fail proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {
    said := linear.comment(issue, "The attempt failed; its plan and log:", files=["#{run.scratch_dir}/plan.md", "#{run.scratch_dir}/run.log"])
    if said != nil { fail("linear.comment: #{said.message}") }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Intake.** `pick_from` names one state or several, `pick_from "Todo"` or
`pick_from "Triage", "Todo"`. The poll goes through them in the order written, each in the
team's manual order from the top, and claims the first issue that passes every gate below;
a name the team has no state for fails the poll, naming it. An issue blocked by another
that is not yet completed, canceled or a duplicate is passed over until it is. An issue the
plugin puts back — its run crashed, or afkd handed it back unrun — goes to the bottom of
the state it was claimed from, so an issue that keeps crashing cannot starve the head of the
queue.

`require_member` claims only an issue assigned or delegated to a user it names: `"me"`,
whoever the credential authenticates as, or a user by display name or email. It takes one
entry or several, `require_member "me", "chen@acme.example"`, and an issue passes if any
entry matches; `"none"` is an issue with neither an assignee nor a delegate. An issue held
by anyone else is passed over, not waited on. `require_label` claims only issues
carrying a label of that exact name, and `without_label` never claims one carrying any of
the labels it names; **deny wins**, so an issue carrying both is skipped.

```conf
import "core"
import "@afkd/linear"

implement :: service(linear) {
  team           "ENG"
  api_key        "REPLACE_ME"
  pick_from      "Todo", "Backlog 📥"
  require_member "me"
  require_label  "afkd/ready"
  without_label  [ "Blocked", "needs design — ask 陳大文" ]

  on_claim proc(issue: linear.Issue) {
    moved := linear.move_to(issue, "In Progress")
    if moved != nil { fail("linear.move_to: #{moved.message}") }
    unlabelled := linear.remove_label(issue, "afkd/ready")
    if unlabelled != nil { fail("linear.remove_label: #{unlabelled.message}") }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Claiming what nobody holds.** `require_member "me", "none"` picks an issue that is free or
already the app's, and `on_claim` delegating it to `"me"` makes it the app's. So the issue
still passes under `"me"` when it comes back from another state — answered out of Needs
Info, or moved back from Backlog — and an issue another app or a person holds is never
touched. **Each computer gets its own app**: `"me"` is the app, so two afkd on one app both
read every issue the app holds as their own, and both fetch and renew a token for that one
app.

```conf
import "core"
import "core:env"
import "@afkd/linear"

LINEAR_CLIENT_ID     :: env.get("LINEAR_CLIENT_ID") ?? ""
LINEAR_CLIENT_SECRET :: env.get("LINEAR_CLIENT_SECRET") ?? ""

develop :: service(linear) {
  team           "ENG"
  client_id      LINEAR_CLIENT_ID
  client_secret  LINEAR_CLIENT_SECRET
  pick_from      "Todo"
  require_member "me", "none"

  on_claim proc(issue: linear.Issue) {
    delegated := linear.delegate(issue, "me")
    if delegated != nil { fail("linear.delegate: #{delegated.message}") }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Grooming.** With `discuss_with` set, an issue is claimed on **first sight** — afkd has
never commented on it — or, once afkd has spoken, when its **tail** (the comments posted
after afkd's own last comment) carries a comment from an allowed author.
`discuss_with [ "anyone" ]` allows any author but afkd itself; a list of users allows only
those, each `"me"`, a display name or an email. The whole tail is read, so a comment from
someone not on the list cannot hide one from someone who is. afkd's claim and park markers
are bookkeeping, not speech, so neither moves the boundary. If a turn's agent posts nothing,
the plugin posts one terse comment so the issue does not fire again on the next poll.

```conf
import "core"
import "@afkd/linear"

groom :: service(linear) {
  team         "ENG"
  api_key      "REPLACE_ME"
  pick_from    "Triage"
  discuss_with [ "chen@acme.example", "me" ]

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**The clarification gate.** A run on an issue has a third outcome between finish and fail:
when the issue is too ambiguous to build — or its premise does not hold at all — the agent
can **ask first**. Give an agent the `@afkd/linear/linear` skill (see
[above](#name-the-skill)); its `ask.py` posts the agent's message on the issue and writes the
marker `park` into the run's scratch dir (`$AFKD_SCRATCH_DIR`). The plugin reads that marker
at the end of the run and *parks* the issue instead of failing it: it adds the `Needs Info`
label, posts an `[afkd-park]` marker naming the service, releases the claim, and **moves
nothing** — the issue keeps its state, its place and its unspent attempts. `on_fail` does
not run, and `on_park` does, for extras.

The label is also how afkd **finds the issue again**. Once a beat, before it reads
`pick_from`, the plugin asks Linear which of the team's issues carry `Needs Info`; one whose
thread has gained a comment afkd did not write, newer than afkd's own last word, is claimed
again right there — wherever it sits — the label and the marker come off, and the reply
arrives under **New comments** in the fresh brief. `without_label` still vetoes it, and a
blocker added while it waited still holds it back. An issue goes back only to the service
its `[afkd-park]` marker names; one parked by a service no longer configured is taken over
by whichever service finds it, with one line in the log.

A **multi-step** run must stop at the ask itself, so nothing runs past the question:

```conf
import "core"
import "core:fs"
import "vendor:claude"
import "@afkd/linear"

// The agent the service below calls. Minimal, so this snippet validates on its own.
builder :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/linear/linear" ]
}

widgets :: service(linear) {
  team      "ENG"
  api_key   "REPLACE_ME"
  pick_from "Todo"

  on_claim proc(issue: linear.Issue) {
    moved := linear.move_to(issue, "In Progress")
    if moved != nil { fail("linear.move_to: #{moved.message}") }
  }
  on_park proc(run: core.Run, issue: linear.Issue, outcome: core.Outcome) {
    said := linear.comment(issue, "parked after #{outcome.duration} — waiting on you")
    if said != nil { fail("linear.comment: #{said.message}") }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: core.Run, issue: linear.Issue) {
    _ = builder <- "build the issue; ask through the linear skill if unclear"
    if fs.is_file("#{run.scratch_dir}/park") { fail("parked: awaiting a human reply") }
    // reviewer and commit steps below never run when the agent asked
  }
}
```

The `fail` aborts the run; the plugin reads the marker and parks the issue instead. The
scratch dir is **outside** any `with git.worktree(…)` copy, so the marker survives the copy
being torn down, and agent, gate and plugin all name the same file.

## Where it differs from `@afkd/trello`

- **States, not lists.** An issue sits in a workflow state, read in the team's manual order
  where a card sits in a list at a position. `move_to` takes a state's name and leaves the
  place in it to Linear; there is no `at=`.
- **Sub-issues, not checklists.** An issue's parts are its sub-issues: `list_sub_issues`
  and `move_sub_issue` where trello ticks a checklist item, and the brief lists them with
  their states and names the issue's own parent.
- **Blocked issues wait.** An issue blocked by one that is not yet completed, canceled or a
  duplicate is never claimed, and its handle carries `blocked_by`.
- **`require_member` reads the assignee and the delegate**, so an issue delegated to an
  app is the app's, and `"me"` is the app when it is one. It also takes `"none"`, an issue
  nobody holds, and `delegate` hands an issue to an app.
- **No `min_age`.**
- **Files are links, not attachments.** A file is a markdown link in the description or a
  comment, so `files=` on `comment` and `describe` puts one up and `fetch_files` reads every
  one back, where trello has `attach` and `fetch_attachment`.
- **The watermark is in milliseconds.** `[afkd-ran]`'s `upto=` is epoch milliseconds, since
  Linear stamps a comment to the millisecond.
- **The skill's scripts are named for Linear:**

  | `@afkd/trello`          | `@afkd/linear`          |
  |-------------------------|-------------------------|
  | `set_name.py`           | `set_title.py`          |
  | `move_card.py`          | `move_issue.py`         |
  | `archive_card.py`       | `archive_issue.py`      |
  | `create_card.py`        | `create_issue.py`       |
  | `list_checklists.py`    | `list_sub_issues.py`    |
  | `check_item.py`         | `move_sub_issue.py`     |
  | `list_lists.py`         | `list_states.py`        |
  | `list_cards.py`         | `list_issues.py`        |
  | `list_attachments.py`   | `list_files.py`         |
  | `fetch_attachment.py`   | `fetch_file.py`         |
  | `post_attachment.py`    | `post_file.py`          |

  The rest keep their names. A state or a label is named by its id or its exact name in one
  argument, where trello takes an id or `--name`.
