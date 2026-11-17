# `@afkd/linear`

A **provider** plugin with one trigger kind, [`service(linear)`](#servicelinear-issues): it
fires when an issue lands in a watched workflow state of a Linear team, and gives the
service's slots the actions that move, label, delegate, comment on, describe and file issues as work
starts, finishes, or fails, that work an issue's sub-issues, and that put a file on an issue
and fetch its files back. It also ships the `linear` skill an agent works the issue through:
read and post comments, fetch and put up files, move the issue and its sub-issues, label
it, file follow-up issues, and ask a question that parks the issue until a human replies.

Beside it, the plugin gives a config the reads of a whole workspace, each through the
[`linear.Workspace`](#reading-linear-through-a-linearworkspace) it is passed: the issues any
filters keep, an issue's comments and files, the viewer, users, teams, states, labels,
projects and cycles. Through the same workspace a config
[writes](#writing-linear-through-a-linearworkspace) an issue's fields, moves it to another
team and places it in its state, relates it to other issues, links urls and subscribes users
on it, archives it, puts it in the trash or takes it back out, and writes, edits, resolves and
deletes its comments.

It does for a Linear team what [`@afkd/trello`](../trello) does for a board: the same
claim, watermark, attempt and park comments on the issue, the same brief and the same run
environment, mapped onto Linear's states, labels, assignees and sub-issues. The places it
behaves differently are listed [at the end](#where-it-differs-from-afkdtrello). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[the afk language](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, released as a finished binary.

## Install it

```console
$ afkd install @afkd/linear
```

afkd downloads the release built for this host's platform — `x86_64-linux` or
`aarch64-linux`, a static binary that runs on any Linux of that CPU — and places it as it
is: the manifest, the `afkd-linear` binary and its `skills/`, nothing else. Nothing is
compiled on your host, and no Rust toolchain is needed there. Installing it needs **afkd
0.2.693 or newer**, the first whose manifests declare a `list[str]` an action returns;
`afkd update @afkd/linear` moves an installed copy onto the next release.

From a checkout of this repository, build and pack it the way the release does, then name
the packed directory:

```console
$ cargo build --release --locked --manifest-path @afkd/linear/Cargo.toml
$ tools/pack-release linear /tmp/afkd-release @afkd/linear/target/release/afkd-linear
$ afkd install /tmp/afkd-release/afkd-linear
```

The configs below are written in the afk language of **afkd 0.2.695 or newer**, the
first that reads a required environment variable where it is written,
`env.get(…) ?? fail(…)`; an older afkd still runs the plugin, but refuses the examples
when they load.

A config file uses the plugin by importing it, and then names it by its leaf, `linear`:

```conf
import "afkd"
import "std:env"
import "@afkd/linear"

LINEAR_API_KEY :: env.get("LINEAR_API_KEY") ?? fail("LINEAR_API_KEY is unset")

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
    match linear.move_to(issue, "In Progress") {
      .ok(_) => { }
      .err(e) => fail("linear.move_to: #{e.message}")
    }
  }
  on_done proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome) {
    match linear.move_to(issue, "In Review") {
      .ok(_) => { }
      .err(e) => fail("linear.move_to: #{e.message}")
    }
    match linear.comment(issue, "afkd landed this issue in #{outcome.duration}.") {
      .ok(_) => { }
      .err(e) => fail("linear.comment: #{e.message}")
    }
  }
  on_park proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome) {
    match linear.comment(issue, "parked after #{outcome.duration}: waiting for a reply.") {
      .ok(_) => { }
      .err(e) => fail("linear.comment: #{e.message}")
    }
  }
  on_fail proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome) {
    match linear.move_to(issue, "Backlog") {
      .ok(_) => { }
      .err(e) => fail("linear.move_to: #{e.message}")
    }
    match linear.add_label(issue, "Problem") {
      .ok(_) => { }
      .err(e) => fail("linear.add_label: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) { task() }
}
```

The credential comes from the daemon's environment, so it never sits in the file. Each
action names the issue it acts on — the `issue` its slot is passed — so it reads the same
wherever it is written.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
import "ai:claude"

// The agent a `service(linear)` runs: the skill is what lets it work the issue.
builder :: claude.agent(model="sonnet", skills=[ "@afkd/linear/linear" ])
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
| `team`           | `str`          | the team's key, the prefix of every identifier it holds (`ENG`); required |
| `api_key`        | `str`          | a Linear API key; exactly one of it or the app's two keys below |
| `client_id`      | `str`          | an OAuth app's client id, with `client_secret` |
| `client_secret`  | `str`          | the OAuth app's client secret          |
| `base_url`       | `str`          | API root; defaults to `https://api.linear.app` (a test seam) |
| `pick_from`      | `list[str]`    | the workflow states new issues are drawn from, by name, in the order written; required |
| `require_member` | `list[str]`    | `"me"`, users by display name or email, or `"none"`: only claim issues assigned or delegated to one of them, `"none"` an issue nobody holds; default unset |
| `require_label`  | `str`          | only claim issues carrying a label of this exact name; default unset |
| `without_label`  | `list[str]`    | never claim issues carrying any of these labels (deny wins over `require_label`); default unset |
| `discuss_with`   | `list[str]`    | `[ "anyone" ]` or users, `[ "chen@acme.example", "me" ]`: claim an issue on first sight or when an allowed author comments after afkd last did; default unset |
| `max_attempts`   | `int`          | retries per issue; default `1`         |
| `every`          | `duration`     | how often the team is polled; default `30s`; a range `2m~3m` jitters |

The last two are afkd's own, read by afkd for every kind that claims its work.

The service's slots are code ([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/)), each a `proc`
passed the run (all but `on_claim`, which runs before one exists), the claimed issue, and
after the run its outcome — by position, and a slot writes every one of them, in order,
each with its type:

| Slot                                                                         | Runs                                            |
|------------------------------------------------------------------------------|-------------------------------------------------|
| `on_run proc(run: afkd.Run, issue: linear.Issue)`                            | the run itself                                  |
| `on_claim proc(issue: linear.Issue)`                                         | once the issue is claimed, before the run       |
| `on_done proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome)`    | after a run that finished                       |
| `on_park proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome)`    | after a run that ended waiting on a human reply |
| `on_fail proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome)`    | after a run that failed                         |

`run` is an `afkd.Run` (`id`, `scratch_dir`) and `outcome` an `afkd.Outcome` (`duration`,
`error`). `issue` is a **`linear.Issue`**, the plugin's handle for the issue, with every
field [its table](#linearissue) lists: its identifier as `id`, and as `key` afkd's claim key
for it.

A config passes an issue around and compares it, but never builds one. The plugin reads the
issue whole once its claim is won and sends every field with the claim, and they hold for the
whole run. A description too long for afkd's 64 KiB plugin line beside the brief is cut in
the handle's `description`, ending in a line saying so; the brief keeps it whole when the two
then fit.

**Credentials.** A personal API key acts as the user who made it. An OAuth app's
`client_id` and `client_secret` are traded for the app's own token with Linear's
`client_credentials` grant, asking for `read,write,app:assignable,app:mentionable`, and the
token is fetched again before it expires: the app is then the actor on every comment, and
`"me"` names the app. Exactly one of the two is written.

```conf
import "afkd"
import "std:env"
import "@afkd/linear"

LINEAR_CLIENT_ID     :: env.get("LINEAR_CLIENT_ID") ?? ""
LINEAR_CLIENT_SECRET :: env.get("LINEAR_CLIENT_SECRET") ?? fail("LINEAR_CLIENT_SECRET is unset")

triage :: service(linear) {
  team           "ENG"
  client_id      LINEAR_CLIENT_ID
  client_secret  LINEAR_CLIENT_SECRET
  pick_from      "Triage"
  require_member "me"

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
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
| `linear.remove_label(issue, "<label>")`                      | take a label off; a name with no label, or a label the issue does not carry, is a no-op |
| `linear.delegate(issue, "<to>")`                             | delegate the issue to `"me"`, the app, or to an app's user by display name or email |
| `linear.undelegate(issue)`                                   | take the issue's delegate off; an issue with no delegate is left as it is |
| `linear.create_issue(issue, "<title>", "<description>", state="<state>")` | file a new issue in the team, in the team's default state when `state` is left out |
| `linear.create_sub_issue(issue, "<title>", "<description>", state="<state>")` | file a new sub-issue of the issue, as `create_issue` does |
| `linear.archive(issue)`                                      | archive the issue                       |
| `linear.list_comments(issue, "<to>")`                        | write the thread, oldest first as the brief renders it, to `to` |
| `linear.read_description(issue, "<to>")`                     | write the description as Linear holds it now to `to` |
| `linear.list_sub_issues(issue, "<to>")`                      | write the sub-issues, a line each with its state, to `to` |
| `linear.move_sub_issue(issue, "<sub-issue>", "<state>")`     | move the sub-issue, by identifier, to a workflow state |
| `linear.fetch_files(issue, "<to>")`                          | write every file the issue links into the directory `to`, numbered |

The issue is a `linear.Issue` and nothing else: `linear.move_to("Done")`, with no issue, is
refused when the config loads, and so is an action the plugin does not declare. These reads
write what they read to the absolute path `to`, through a temp file renamed onto it; the
[workspace reads](#reading-linear-through-a-linearworkspace) hand theirs back.

Each action returns a `result[nil, linear.Error]`, like any call that can fail: `.ok` when
it landed, and when Linear refused it — a state the team does not have, a sub-issue that is
not the issue's — an `.err` with the plugin's sentence. A failing `on_claim` gives the issue
back and fails the run; a failing post-run slot is logged and changes nothing. afkd never
retries a slot's action; the plugin retries what Linear could not take.

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
import "afkd"
import "@afkd/linear"

develop :: service(linear) {
  team      "ENG"
  api_key   "REPLACE_ME"
  pick_from "Todo"

  on_claim proc(issue: linear.Issue) {
    match linear.fetch_files(issue, "/srv/acme/widgets/files") {
      .ok(_) => { }
      .err(e) => fail("linear.fetch_files: #{e.message}")
    }
  }
  on_fail proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome) {
    match linear.comment(
      issue,
      "The attempt failed; its plan and log:",
      files=[ "#{run.scratch_dir}/plan.md", "#{run.scratch_dir}/run.log" ],
    ) {
      .ok(_) => { }
      .err(e) => fail("linear.comment: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Intake.** `pick_from` names one state or several, `pick_from "Todo"` or
`pick_from "Triage", "Todo"`. The poll goes through them in the order written, each in the
team's manual order from the top, and claims the first issue that passes every gate below;
a name the team has no state for fails the poll, naming it. An issue blocked by another
that is not yet completed, canceled or a duplicate is passed over until it is. An issue the
plugin puts back — its run crashed, its `on_claim` failed or its finish could not be
delivered — goes to the bottom of the state it was claimed from, so an issue that keeps
crashing cannot starve the head of the queue. An issue afkd handed back because its service
stopped before the run started keeps its place, with its claim comment deleted.

`require_member` claims only an issue assigned or delegated to a user it names: `"me"`,
whoever the credential authenticates as, or a user by display name or email. It takes one
entry or several, `require_member "me", "chen@acme.example"`, and an issue passes if any
entry matches; `"none"` is an issue with neither an assignee nor a delegate. An issue held
by anyone else is passed over, not waited on. `require_label` claims only issues
carrying a label of that exact name, and `without_label` never claims one carrying any of
the labels it names; **deny wins**, so an issue carrying both is skipped.

```conf
import "afkd"
import "@afkd/linear"

implement :: service(linear) {
  team           "ENG"
  api_key        "REPLACE_ME"
  pick_from      "Todo", "Backlog 📥"
  require_member "me"
  require_label  "afkd/ready"
  without_label  [ "Blocked", "needs design — ask 陳大文" ]

  on_claim proc(issue: linear.Issue) {
    match linear.move_to(issue, "In Progress") {
      .ok(_) => { }
      .err(e) => fail("linear.move_to: #{e.message}")
    }
    match linear.remove_label(issue, "afkd/ready") {
      .ok(_) => { }
      .err(e) => fail("linear.remove_label: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
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
import "afkd"
import "std:env"
import "@afkd/linear"

LINEAR_CLIENT_ID     :: env.get("LINEAR_CLIENT_ID") ?? ""
LINEAR_CLIENT_SECRET :: env.get("LINEAR_CLIENT_SECRET") ?? fail("LINEAR_CLIENT_SECRET is unset")

develop :: service(linear) {
  team           "ENG"
  client_id      LINEAR_CLIENT_ID
  client_secret  LINEAR_CLIENT_SECRET
  pick_from      "Todo"
  require_member "me", "none"

  on_claim proc(issue: linear.Issue) {
    match linear.delegate(issue, "me") {
      .ok(_) => { }
      .err(e) => fail("linear.delegate: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
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
import "afkd"
import "@afkd/linear"

groom :: service(linear) {
  team         "ENG"
  api_key      "REPLACE_ME"
  pick_from    "Triage"
  discuss_with [ "chen@acme.example", "me" ]

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
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
import "afkd"
import "ai:claude"
import "std:fs"
import "@afkd/linear"

// The agent the service below calls. Minimal, so this snippet validates on its own.
builder :: claude.agent(model="sonnet", skills=[ "@afkd/linear/linear" ])

widgets :: service(linear) {
  team      "ENG"
  api_key   "REPLACE_ME"
  pick_from "Todo"

  on_claim proc(issue: linear.Issue) {
    match linear.move_to(issue, "In Progress") {
      .ok(_) => { }
      .err(e) => fail("linear.move_to: #{e.message}")
    }
  }
  on_park proc(run: afkd.Run, issue: linear.Issue, outcome: afkd.Outcome) {
    match linear.comment(issue, "parked after #{outcome.duration} — waiting on you") {
      .ok(_) => { }
      .err(e) => fail("linear.comment: #{e.message}")
    }
  }

  work_dir "/srv/acme/widgets"
  on_run proc(run: afkd.Run, issue: linear.Issue) {
    _ = builder("build the issue; ask through the linear skill if unclear")
    if fs.is_file("#{run.scratch_dir}/park") { fail("parked: awaiting a human reply") }
    // reviewer and commit steps below never run when the agent asked
  }
}
```

The `fail` aborts the run; the plugin reads the marker and parks the issue instead. The
scratch dir is **outside** any `with git.worktree(…)` copy, so the marker survives the copy
being torn down, and agent, gate and plugin all name the same file.

## Reading Linear through a `linear.Workspace`

Beside the issue a service claims, a config reads a whole workspace: the issues any set of
filters keeps, an issue's comments and the files it links, the viewer, the users, teams,
states, labels, projects and cycles, and a file downloaded. Each read takes a
`linear.Workspace` first, the credential it reaches Linear with, and reaches Linear through
nothing else: no read takes a service's settings, so it reads the same from an interval
service's run, from another plugin's service and from a `service(linear)`'s slots. Called
outside the plugin's own services, a read is a child afkd starts for that one call.

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/linear"

ACME :: linear.Workspace{ api_key: env.get("LINEAR_API_KEY") ?? fail("LINEAR_API_KEY is unset") }

standup :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match linear.issues(ACME, team=[ "ENG", "OPS" ], state=[ "Todo" ], order=[ .priority, .created ]) {
      .ok(issues) => {
        for issue in issues {
          if issue.priority == .urgent {
            $ echo #{issue.id} #{issue.title} #{issue.assignee ?? "nobody"}
          }
        }
      }
      .err(e) => fail("linear.issues: #{e.message}")
    }
  }
}
```

### `linear.Workspace`

A struct the plugin declares, built as `linear.Workspace{ … }`:

| Field           | Type                              | What it is                                    |
|-----------------|-----------------------------------|-----------------------------------------------|
| `api_key`       | `str?`, secret                    | a personal API key                            |
| `client_id`     | `str?`                            | an OAuth app's client id                      |
| `client_secret` | `str?`, secret                    | its secret, traded for the app's own token with the `client_credentials` grant |
| `base_url`      | `str = "https://api.linear.app"`  | a stand-in's address in tests                 |

- Exactly one of `api_key` or the pair `client_id` and `client_secret` is set and not empty;
  any other is an `.err` of every read naming the fields, such as `issues: the
  linear.Workspace sets api_key beside client_id; set one or the other`.
- An app's token is kept in the plugin's own directory by client id, readable by its owner
  alone, so the children afkd starts for each call share it. It is fetched anew shortly
  before it expires, or once Linear refuses it, and never per call.
- A credential reaches exactly one workspace, so two workspaces are two `Workspace` values,
  used side by side.
- Every handle a read hands back has the `key` `linear:<organization uuid>/<object uuid>`. A
  read finds the object by it, so a handle keeps working after its issue moves team and its
  identifier changes, and `==` on two handles compares it. A read passed a handle of another
  workspace than its `Workspace` reaches is an `.err` naming both url keys, `comments: issue
  is ACME-12 of workspace acme, and the linear.Workspace reaches globex`.
- The issue a `service(linear)` claimed carries afkd's claim key instead, so a read in its
  slots passed it finds the issue the service holds; it is not `==` a handle a read hands
  back of the same issue.

### The enums

| Enum                | Members                                                                    |
|---------------------|----------------------------------------------------------------------------|
| `linear.Priority`   | `none`, `urgent`, `high`, `medium`, `low`: Linear's 0 to 4                 |
| `linear.State_Type` | `triage`, `backlog`, `unstarted`, `started`, `completed`, `canceled`; a state Linear marks `duplicate` is `canceled` |
| `linear.Sort`       | `manual`, `team`, `state`, `priority`, `assignee`, `project`, `cycle`, `due`, `estimate`, `title`, `created`, `updated`, `completed`: keys of Linear's own sort on issues, at most three to an `order` |

A field, a filter and a local are typed by them alike: `issue.priority == .urgent`,
`priority=[ .urgent, .high ]`.

### `linear.Issue`

Every field is sent, filled wherever the plugin hands an issue out, the claimed one
included. A field that may hold nothing is `T?`, and is `nil` when it does.

| Field                                                     | Type                 | What it is |
|-----------------------------------------------------------|----------------------|------------|
| `id`                                                      | `str`                | the identifier as it is now, `ENG-123` |
| `key`                                                     | `str`                | `linear:<organization uuid>/<issue uuid>`, or afkd's claim key for a claimed issue |
| `uuid`                                                    | `str`                | Linear's id for it, the same after it moves team and its identifier changes |
| `workspace`                                               | `str`                | the workspace's url key, `acme` |
| `team`                                                    | `str`                | the team's key |
| `title`, `url`                                            | `str`                | |
| `description`                                             | `str`                | markdown, `""` when empty |
| `branch`                                                  | `str`                | Linear's git branch name for it |
| `state`                                                   | `str`                | the workflow state's name |
| `state_type`                                              | `linear.State_Type`  | |
| `priority`                                                | `linear.Priority`    | |
| `estimate`                                                | `int?`               | |
| `due`                                                     | `str?`               | `YYYY-MM-DD` |
| `assignee`, `delegate`                                    | `str?`               | display names |
| `creator`                                                 | `str?`               | `nil` for one an integration filed |
| `project`, `milestone`                                    | `str?`               | names |
| `cycle`                                                   | `int?`               | number |
| `labels`                                                  | `list[str]`          | names |
| `parent`                                                  | `str?`               | identifier |
| `sub_issues`                                              | `list[str]`          | identifiers, in Linear's order |
| `blocks`, `blocked_by`, `related`, `similar`, `duplicates` | `list[str]`         | identifiers, one list per relation, done or not |
| `duplicate_of`                                            | `str?`               | identifier |
| `links`                                                   | `list[str]`          | the urls of its links (Linear's attachments) |
| `subscribers`                                             | `list[str]`          | display names |
| `created_at`, `updated_at`                                | `time.Time`          | |
| `completed_at`, `canceled_at`                             | `time.Time?`         | `nil` unless it was |
| `archived`, `trashed`                                     | `bool`               | a trashed issue is archived too |

A field that points at an issue carries its identifier, since a field cannot hold a handle;
`linear.issues(ws, id=[ … ])` turns identifiers into handles in one call.

### The other handles

| Handle           | Fields |
|------------------|--------|
| `linear.Comment` | `id` (its uuid), `issue` (identifier), `body`, `author: str?` (a display name, or an integration's or an external user's name), `reply_to: str?` (the id of the comment it answers), `resolved_at: time.Time?`, `created_at: time.Time`, `edited_at: time.Time?`, `url` |
| `linear.Team`    | `id` (its key, `ENG`), `name` |
| `linear.State`   | `id`, `name`, `type: linear.State_Type`, `team` |
| `linear.Label`   | `id`, `name`, `group: str?`, `team: str?` (`nil` for a workspace label) |
| `linear.User`    | `id`, `name` (display name), `full_name`, `email`, `app: bool` |
| `linear.Project` | `id`, `name`, `teams: list[str]`, `milestones: list[str]` (in order), `url` |
| `linear.Cycle`   | `id`, `number: int`, `name: str?`, `team`, `starts_at`, `ends_at: time.Time`, `active: bool` |

### The reads

| Call                                  | Gives |
|---------------------------------------|-------|
| `linear.issues(ws, ~filters…)`        | `list[linear.Issue]` matching every filter given, workspace-wide unless `team` says |
| `linear.comments(ws, issue)`          | `list[linear.Comment]`, oldest first, each reply right after the comment it answers |
| `linear.me(ws)`                       | the `linear.User` the credential acts as |
| `linear.users(ws)`                    | `list[linear.User]`, apps included, disabled accounts not |
| `linear.teams(ws)`                    | `list[linear.Team]` the credential can reach |
| `linear.states(ws, team)`             | `list[linear.State]`, in workflow order |
| `linear.labels(ws, ~team)`            | `list[linear.Label]`: the workspace's, and the team's when given |
| `linear.projects(ws, ~team)`          | `list[linear.Project]`, every one or the team's |
| `linear.cycles(ws, team)`             | `list[linear.Cycle]`, oldest first |
| `linear.files(ws, issue)`             | `list[str]`: the urls of every file Linear hosts that its description and comments link, the description's first, then the comments' oldest first; a link in one of afkd's own control markers (its claim, attempt, ran and park comments) is left out, though `comments` returns the markers themselves |
| `linear.download_file(ws, url, to)`   | nil; saves the file at `url` to the absolute path `to`, sending the credential Linear's file host asks for, and to no other host |

A team is named by its key. An unknown one is refused by `states` and `cycles`, `linear
resolve team: no team with the key 'QA'`, and matches nothing in a filter.

### `linear.issues`'s filters

All are named-only and may be left out. Each is named after the field it reads, a list on a
single-valued field meaning any of them.

| Filter                                         | Type                       | Keeps |
|------------------------------------------------|----------------------------|-------|
| `id`                                           | `list[str]`                | these identifiers or uuids |
| `team`                                         | `list[str]`                | |
| `state`                                        | `list[str]`                | |
| `state_type`                                   | `list[linear.State_Type]`  | |
| `priority`                                     | `list[linear.Priority]`    | |
| `labels`                                       | `list[str]`                | carrying every one |
| `without_labels`                               | `list[str]`                | carrying none |
| `assignee`, `delegate`                         | `list[str]`                | a user by display name or email, `"me"`, or `"none"` for nobody |
| `project`, `milestone`                         | `list[str]`                | |
| `cycle`                                        | `list[int]`                | |
| `parent`                                       | `list[str]`                | sub-issues of these identifiers or uuids |
| `blocked`                                      | `bool`                     | `true` an issue another open issue blocks, `false` one no open issue blocks |
| `text`                                         | `str`                      | title or description containing it, case ignored |
| `created_after`, `created_before`, `updated_after`, `updated_before`, `completed_after`, `completed_before`, `canceled_after`, `canceled_before` | `time.Time` | |
| `due_after`, `due_before`                      | `str`                      | `YYYY-MM-DD` |
| `include_archived`                             | `bool = false`             | archived and trashed ones too |
| `order`                                        | `list[linear.Sort] = [ .team, .state, .manual ]` | Linear's own sort, at most three keys, each breaking the ties of the one before: `.manual` top to bottom as Linear shows a state, the dates newest first, the rest as Linear orders them |
| `limit`                                        | `int = 50`                 | 1 to 250, Linear's largest page |

- A list that names nothing is refused rather than read as keeping nothing, ``issues: `team`
  is empty; leave it out to keep every issue``. So is a `limit` outside 1 to 250, a due date
  that is no `YYYY-MM-DD` day of the calendar, and an `id` or a `parent` that is neither an
  identifier nor a uuid.
- An `order` of more than three keys is refused before anything is sent, ``issues: `order`
  names 4 keys, and Linear sorts by at most 3``, for Linear refuses a longer sort.
- A time filter before 1970 reads as 1970 itself, Linear holding no earlier time: an
  `_after` keeps every issue, and a `_before` none.
- An identifier an issue carried before it moved team names nothing; its uuid always finds
  it.
- A user named by no display name or email matches nothing, where `require_member` refuses
  the poll.

**Retries, and the size of a reply.** A read Linear rate limits, answers with a 5xx or does
not answer is tried again within the call's deadline, after the `Retry-After` Linear named,
else after a second, then two, doubling, and is refused with the last reason once no wait
fits; nothing is queued. A reply runs to afkd's 8 MiB for one call. One past it is refused,
`issues: the reply comes to 9437184 bytes, over afkd's 8 MiB for one call; ask for fewer
with limit`.

**The issue actions act on a claimed issue.** `comment`, `move_to` and the rest of the
actions [above](#servicelinear-issues) take the issue a `service(linear)` claimed, and do it
in that service's team. Called from anywhere else they are refused, `comment acts only on an
issue a service(linear) claimed; call it from that service's slots`.

## Writing Linear through a `linear.Workspace`

A config changes an issue as it reads one: each write takes a `linear.Workspace` first and
reaches Linear through nothing else, from any service's run or slots, or in a child afkd
starts for the one call. A write is done now and gives back what it changed as Linear then
holds it, the same handle `linear.issues` or `linear.comments` reads, or it is an `.err` with
the reason. Nothing
is queued: a request Linear rate limits, answers with a 5xx or does not answer is tried
again within the call's deadline, as a read's is.

```conf
import "afkd"
import "std:env"
import "svc:interval"
import "@afkd/linear"

ACME :: linear.Workspace{ api_key: env.get("LINEAR_API_KEY") ?? fail("LINEAR_API_KEY is unset") }

triage :: service(interval) {
  every    1h
  work_dir "/srv/acme/widgets"

  on_run proc(run: afkd.Run) {
    match linear.issues(ACME, team=[ "ENG" ], state=[ "Triage" ], priority=[ .urgent ]) {
      .ok(issues) => {
        for issue in issues {
          match linear.update_issue(ACME, issue, state="Todo", at=.top, assignee=nil) {
            .ok(moved) => {
              $ echo #{moved.id} is at the top of #{moved.state}
            }
            .err(e) => fail("linear.update_issue: #{e.message}")
          }
        }
      }
      .err(e) => fail("linear.issues: #{e.message}")
    }
  }
}
```

### The writes

| Call | Gives |
|------|-------|
| `linear.update_issue(ws, issue, ~title, ~description, ~files, ~state, ~team, ~at, ~after, ~priority, ~estimate, ~due, ~assignee, ~delegate, ~project, ~milestone, ~cycle, ~parent)` | the `linear.Issue` as Linear now holds it; `team` moves it to that team and needs a `state` of that team beside it, and Linear drops what the new team lacks — its old team's labels, its cycle, and a project the new team is not in; `at` and `after` place it in its state, the new one or the one it is in |
| `linear.add_relation(ws, issue, relation, other)`, `linear.remove_relation(ws, issue, relation, other)` | the `linear.Issue`; afterwards `other`'s identifier is, or is no longer, in the issue's field named for `relation` |
| `linear.add_link(ws, issue, url, ~title)`, `linear.remove_link(ws, issue, url)` | the `linear.Issue`; adding a url the issue already links titles that link `title` |
| `linear.add_subscriber(ws, issue, user = "me")`, `linear.remove_subscriber(ws, issue, user = "me")` | the `linear.Issue` |
| `linear.archive_issue(ws, issue)`, `linear.unarchive_issue(ws, issue)` | the `linear.Issue`, its sub-issues archived or taken back with it, as Linear does; `unarchive_issue` also takes back an issue `delete_issue` put in the trash, within Linear's 30 days |
| `linear.delete_issue(ws, issue)` | the `linear.Issue`, `archived` and `trashed`; it goes to Linear's trash with its sub-issues, as Linear does |
| `linear.create_comment(ws, issue, body, ~files, ~reply_to)` | the `linear.Comment`; `reply_to` files it in that comment's thread, and a reply to a reply goes in the thread both are in, since Linear keeps threads one level deep |
| `linear.update_comment(ws, comment, body, ~files)` | the `linear.Comment`, its body rewritten and `edited_at` set |
| `linear.resolve_comment(ws, comment)`, `linear.unresolve_comment(ws, comment)` | the first `linear.Comment` of the comment's thread, which carries `resolved_at`; a reply resolves its thread, as the reply that resolved it |
| `linear.delete_comment(ws, comment)` | nil; a thread's first comment takes its replies with it, as Linear does |

`update_issue`'s parameters after the handle, `add_link`'s `title`, and a comment write's
`files` and `reply_to` are named-only. A parameter of `update_issue` left out leaves its field
as it is, and `nil` clears it.

| Parameter              | Type              | What it is |
|------------------------|-------------------|------------|
| `title`                | `str`             | |
| `description`          | `str`             | markdown; `""` empties it |
| `body`                 | `str`             | a comment's, markdown |
| `files`                | `list[str]`       | absolute paths, each read when called and embedded after the description or the body, each on a paragraph of its own: an image shown, any other file linked; in `update_issue` only beside `description` |
| `state`                | `str`             | a state of the issue's team, or of `team` |
| `team`                 | `str`             | a team's key |
| `at`                   | `.top`, `.bottom` | the place in the state |
| `after`                | `linear.Issue`    | right below it in the state |
| `priority`             | `linear.Priority` | |
| `estimate`             | `int?`            | `0` only on a team that allows a zero estimate |
| `due`                  | `str?`            | `YYYY-MM-DD` |
| `assignee`, `delegate` | `str?`            | a user's display name or email, or `"me"` |
| `project`              | `str?`            | |
| `milestone`            | `str?`            | a milestone of the issue's project, or of `project` |
| `cycle`                | `int?`            | a cycle's number, in the issue's team or in `team` |
| `parent`               | `linear.Issue?`   | |
| `relation`             | `.blocks`, `.blocked_by`, `.related`, `.similar`, `.duplicate_of`, `.duplicates` | the issue's field `other` goes in, or comes out of |
| `other`                | `linear.Issue`    | another issue |
| `url`                  | `str`             | |
| `title`                | `str`             | the link's; with none, Linear titles it |
| `user`                 | `str`             | a user's display name or email, or `"me"` |
| `comment`              | `linear.Comment`  | a comment `linear.comments` or a comment write gave back |
| `reply_to`             | `linear.Comment`  | a comment on the same issue |

**Names.** A team is named by its key, a state, a project or a milestone by its name, a user
by display name or email, or `"me"` for whoever the credential acts as, and a cycle by its
number, each matched exactly. A state and a cycle are looked up in the issue's team, or with
`team` in the new one, and a milestone in the issue's project, or in `project`.

- A name that matches nothing is an `.err` naming what there is, `update_issue: no state
  named "Doing" in team ENG; there are: Triage, Todo, In Progress, Done`.
- A name that matches several is an `.err` listing their ids, `update_issue: 2 users are
  named "sam": <id>, <id>; pass the id of the one you mean in place of the name`. A team,
  a state, a user, a project or a milestone is taken by its id as well as by its name.

**Relations.** Each `relation` is one of Linear's, held by one of the two issues, and shows
in both: `add_relation(ws, a, .blocks, b)` puts `b` in `a.blocks` and `a` in `b.blocked_by`.

| `relation`      | Linear's relation | held by   | `other`'s field |
|-----------------|-------------------|-----------|-----------------|
| `.blocks`       | blocks            | the issue | `blocked_by`    |
| `.blocked_by`   | blocks            | `other`   | `blocks`        |
| `.related`      | related           | either    | `related`       |
| `.similar`      | similar           | either    | `similar`       |
| `.duplicate_of` | duplicate         | the issue | `duplicates`    |
| `.duplicates`   | duplicate         | `other`   | `duplicate_of`  |

`related` and `similar` read the same from either end, so `remove_relation` takes one off
whichever issue holds it. Adding a relation the issue already shows, removing one it does
not, adding a url it already links without a `title`, and removing a url it does not link
send Linear nothing: each is `.ok` with the issue as it is.

**Placing an issue.** `at=.top` puts the issue above the first of its state's issues,
`.bottom` below the last, and `after` right below that issue, halfway to the one below it.
With neither, Linear places it. The issue `after` names must sit in the state the issue ends
up in, `update_issue: after ENG-9 is in Done of ENG, not Todo of ENG`, and be another issue.

**What is refused.** Before anything is sent: `team` without a `state`, `update_issue: team
OPS needs a state of team OPS beside it`; `files` without `description`, `update_issue:
files go only beside description`, and a file that is not absolute, not there or over
10 MiB; `at` beside `after`, `update_issue: at and after cannot both be given`; a `due` that
is no day of the calendar; and a `milestone` beside `project=nil`. Before anything is
changed: a handle of another workspace, as the issue, `after`, `parent`, `other`, the comment
or `reply_to`, named as a read names one; `other` the issue itself, `add_relation: other is
ENG-5 itself`; a `reply_to` on another issue, `create_comment: reply_to <id> is on ENG-7, not
ENG-5`; and every name that matches nothing or several, a `user` as an `assignee` is. An
issue 30 days in the trash is gone for good, and `unarchive_issue` relays Linear's `Entity
not found`, as `delete_comment` does for a comment already deleted. A value Linear itself
refuses, such as an estimate of 0 on a team that does not allow one, is its `.err`, with
Linear's reason.

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
  nobody holds, and `delegate` hands an issue to an app, `undelegate` takes it back.
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
