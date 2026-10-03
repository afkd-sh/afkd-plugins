# `@afkd/gitea`

A **provider** plugin with two trigger kinds. [`service(gitea)`](#servicegitea-issues) turns
open issues on a Gitea repository — or every repository of an org — into afkd runs, and gives
the service's slots the actions that reflect progress back through each issue's assignee,
labels and state. [`service(gitea.pr)`](#servicegiteapr-pull-request-review) runs the review
loop on open pull requests: it fires whenever a human leaves feedback newer than the bot's
last word. It also ships the `gitea` skill an agent uses to answer the issue or the review:
open the pull request, post comments and attachments, and ask a question that parks the
issue until a human replies.

They are afkd's built-in Gitea triggers, moved out of afkd: the same keys, the same claim
markers on the issue or PR, the same claim-journal keys and session threads, the same run
environment and the same brief. A claim the built-in left on a live issue or PR is
recognised, renewed and released by the plugin, and the other way round, so switching from
one to the other strands nothing. The few places the plugin behaves differently are listed
[at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[config language v2](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, built from source when it is installed.

## Install it

```console
$ afkd install @afkd/gitea
```

From a checkout of this repository, name the directory instead:

```console
$ afkd install /path/to/afkd-plugins/@afkd/gitea
```

afkd copies the tree and runs `cargo build --release --locked` in it, so the host needs a
Rust toolchain — the one afkd itself was installed with is enough. The first build fetches
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs).

Run it on **afkd 0.2.241 or newer**, the first that hands a slot the fields this plugin
sends with a claim: this is a manifest v2 plugin, which a
[config language v2](https://afkd.sh/docs/lang-v2/) file imports; afkd passes its slots the
claimed issue as a `gitea.Issue`, or the pull request as a `gitea.Pull_Request`, and sends
each action a slot calls on one as a `call`. afkd 0.2.197 to 0.2.240 install and run it too,
but there a slot reading any field beyond `id` and `key` fails; an older afkd refuses the
manifest when it is installed.

A config file uses the plugin by importing it, and then names it by its leaf, `gitea`:

```conf
import "@afkd/gitea"

GITEA_TOKEN :: env.GITEA_TOKEN

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task.md
}

develop :: service(gitea) {
  base_url      "https://gitea.example.com"
  repo          "acme/widgets"
  token         GITEA_TOKEN
  poll_interval 1m to 3m

  on_claim(run: afkd.Run, issue: gitea.Issue) {
    gitea.assign_me(issue)
    gitea.label_add(issue, "afkd/working")
  }
  on_done(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
    gitea.label_remove(issue, "afkd/working")
    gitea.comment(issue, "Fixed in #{outcome.duration} by #{gitea.me}.")
    gitea.close(issue)
  }
  on_park(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
    gitea.comment(issue, "parked after #{outcome.duration}: waiting for a reply.")
  }
  on_fail(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
    gitea.label_remove(issue, "afkd/working")
    gitea.unassign(issue)
  }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, issue: gitea.Issue) { task() }
}

reviews :: service(gitea.pr) {
  base_url      "https://gitea.example.com"
  repo          "acme/widgets"
  token         GITEA_TOKEN
  author_me     true
  poll_interval 2m to 4m

  on_claim(run: afkd.Run, pr: gitea.Pull_Request) { gitea.pr_label_add(pr, "afkd/working") }
  on_done(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) {
    gitea.pr_label_remove(pr, "afkd/working")
    gitea.pr_comment(pr, "Round done in #{outcome.duration}.")
  }
  on_fail(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) { gitea.pr_label_remove(pr, "afkd/working") }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, pr: gitea.Pull_Request) { task() }
}
```

The token comes from the daemon's environment, so it never sits in the file. Each action
names the item it acts on — the `issue` or `pr` its slot is passed — so it reads the same
wherever it is written, and a slot may act on another item than the one its run claimed.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
// The agent a `service(gitea)` runs: the skill is what lets it answer the issue.
fixer :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/gitea/gitea" ]
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`GITEA_TOKEN`, `GITEA_BASE_URL`, `GITEA_REPO` and `GITEA_ISSUE_NUMBER` every
`service(gitea)` run carries. A `service(gitea.pr)` run carries `GITEA_PR_NUMBER` and
`GITEA_PR_BRANCH` (the PR's head branch, to fetch and check out) in place of
`GITEA_ISSUE_NUMBER`, and the bare PR number under `pr/number` in its scratch directory, so
the same skill reads and answers the PR.

## `service(gitea)` (issues)

Fires for open issues on a Gitea repository — or every repository of an org — and its slots
reflect progress back through the issue's assignee, labels, and state.

An issue is **up for grabs** when it is **assigned to the bot** — the user the `token`
authenticates as (e.g. a dedicated `autocoder` account) — *or* when it carries the optional
`source_label`. Assignment is the seamless path: point the bot's user at an issue and the
next poll picks it up, no label to remember. The `source_label` remains as a classic label
gate for those who want it; either signal suffices.

| Key               | Type           | Notes                                              |
|-------------------|----------------|----------------------------------------------------|
| `base_url`        | `string`       | the Gitea instance base URL                        |
| `repo`            | `string`       | a single repository, `"owner/name"`; **exactly one** of `repo`/`org` |
| `org`             | `string`       | every repository of an org; **exactly one** of `repo`/`org` |
| `token`           | `string`       | personal access token; required                    |
| `source_label`    | `string`       | optional label gate; alternative to assigning bot  |
| `discuss_with`    | `list[string]` | `[ "anyone" ]` or logins, `[ "alice", "bob" ]`: claim an issue on first sight (afkd never commented) or when an allowed author comments after afkd last did; default unset |
| `follow_comments` | `duration`     | re-read the issue this often **while its run is in flight**, delivering new comments to the working agent; a range `30s to 90s` jitters; default unset (no mid-run watch) |
| `max_attempts`    | `int`          | retries per issue; default `1`                     |
| `poll_interval`   | `duration`     | default `30s`; a range `2m to 3m` jitters          |

The last three are afkd's own, read by afkd for every kind that claims its work.

**Exactly one** of `repo` (a single `owner/name`) or `org` (every repo of an org) is
required — naming neither or both is refused when the service starts. The slots only manage
the issue's *status* — its assignee, labels, and state. The **claim** itself is a
`[afkd-claim]` marker comment the plugin posts and releases on its own; no slot holds or
releases it. afkd keeps that marker **alive while the run is**, asking the plugin to edit it
every few minutes, so a run past an hour still holds its issue and a second instance loses
the race rather than double-claiming it; only a marker nobody is renewing any more ages out.
Opening the PR and posting the reply comment are the **agent's** job, done through the
`@afkd/gitea/gitea` skill, not through slot actions.

**The claim label.** The plugin holds its re-pick gate in `afkd/claimed`: it adds the label
itself when a claim is won, and an issue carrying it is never picked up again (a human, or a
slot's `gitea.label_remove(issue, "afkd/claimed")`, removes it to retry). It **creates that
label** — and `afkd/awaiting-reply` — in the repository when they are missing, as plain
non-exclusive labels. Do not redefine either as an **exclusive** scoped label: Gitea strips
an exclusive label the moment another label in the same scope is added, so the gate would
vanish under your own `on_claim` and the issue would be claimed again on every poll. A
plugin that finds `afkd/claimed` already defined exclusive refuses to run and says so.

**Cadence.** Polls the forge every `poll_interval` for open issues **assigned to the bot**
(or carrying `source_label`, if set) across a single `repo` or every repo of an `org`; a
service works **one issue at a time, to completion**, and `max_attempts` bounds the
per-issue retries.

```conf
import "@afkd/gitea"

widgets :: service(gitea) {
  base_url "https://gitea.example.com"
  org      "acme"
  token    "REPLACE_ME"

  on_claim(run: afkd.Run, issue: gitea.Issue) { gitea.assign_me(issue) }
  on_done(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) { gitea.close(issue) }
  on_fail(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) { gitea.unassign(issue) }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, issue: gitea.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**The clarification gate.** A run has a third outcome between finish and fail: when the
issue is too ambiguous to implement, the agent can **ask first**. Give an agent the
`@afkd/gitea/gitea` skill; its **ask** action posts the agent's questions on the issue and
writes the marker `park` into the run's scratch dir (`$AFKD_SCRATCH_DIR`). The plugin reads
that marker at the end of the run and *parks* the issue instead of failing it — drops the
claim, labels it `afkd/awaiting-reply`, removes the bot from the assignees, **without
closing it**. The label is managed by the plugin itself, so the gate holds even with no
`on_park` slot; `on_park` is only for extras (a custom label, say). A parked issue is
**re-claimed automatically on a later poll once a human replies** with a comment newer than
the bot's last word — a fresh run starts with the answer in its brief. The exchange repeats
until the agent has what it needs, then it proceeds to `on_done` as usual.

The marker lives in scratch, not the working dir, so the gate works the same in-repo and
under a `with git.worktree(…)` copy (scratch is outside the copy the engine tears down),
and a fresh scratch dir per attempt means no marker can outlive the run that wrote it. One
requirement remains:

- **Stop the run at the ask, yourself.** The marker parks the issue, but the engine does
  not halt the workflow for you — so in a multi-step workflow, gate the rest on the marker
  so nothing runs after the ask:

  ```conf
  import "@afkd/gitea"

  // The agent the service below calls. Minimal, so this snippet validates on its own.
  fixer :: agent(claude) {
    model  "sonnet"
    skills [ "@afkd/gitea/gitea" ]
  }

  widgets :: service(gitea) {
    base_url "https://gitea.example.com"
    repo     "acme/widgets"
    token    "REPLACE_ME"

    on_park(run: afkd.Run, issue: gitea.Issue, outcome: afkd.Outcome) {
      gitea.comment(issue, "parked after #{outcome.duration} — waiting on you")
    }

    work_dir "/srv/acme/widgets"
    on_run(run: afkd.Run, issue: gitea.Issue) {
      fixer <- "fix the issue; run the gitea skill's ask action if unclear"
      if fs.is_file("#{run.scratch_dir}/park") { fail "parked: awaiting a human reply" }
      // reviewer / commit / PR steps below never run when the agent asked
    }
  }
  ```

  The `fail` aborts the run; the plugin reclassifies that fault into the park. A
  single-step workflow (the ask is the last thing) needs no guard — the run simply ends.

**Grooming.** Without `discuss_with`, an issue that stays assigned to the bot (or keeps its
`source_label`) is a candidate on **every** poll — so a run that finished cleanly re-fires,
and the bot ends up answering itself. `discuss_with` is the reply gate that stops that.
With it set, an issue is claimed on **first sight** — afkd has never commented on it, so a
freshly-assigned issue with a description and **no comments** is picked up on the next poll
(assignment is the opt-in) — or, once afkd has spoken, when its **tail** (the comments
posted after afkd's own last comment) carries a comment from an allowed author.
`discuss_with [ "anyone" ]` allows any author but the bot itself; `discuss_with [ "alice",
"bob" ]` allows only those Gitea logins (the bot's own login is struck from the list however
it is written, so it can never answer itself); on first sight the allow-list does not apply.
The tail boundary is decided by comment **author**, not marker text, so any comment posted
as the bot bounds it, and the whole tail is scanned — a drive-by comment from a disallowed
author cannot mask a still-unanswered allowed one. The retained replies are what the
regenerated brief hands the agent, filtered by the same allow-list.

Every gated turn ends with afkd as the last speaker: if the agent posts nothing during the
run, the plugin posts one terse backstop comment (`reviewed, nothing to add`, `awaiting a
human reply`, or `run did not complete: …`) so the issue does not re-fire on the next poll.
A comment a post-run slot calls for lands after that backstop — see
[below](#where-it-differs-from-the-built-in). Omitting the key leaves the claim path
unchanged — no comments are read at all outside the `afkd/awaiting-reply` re-arm above;
`[ "anyone" ]` is an explicit value, not the same as omitting it.

```conf
import "@afkd/gitea"

groom :: service(gitea) {
  base_url     "https://gitea.example.com"
  repo         "acme/widgets"
  token        "REPLACE_ME"
  discuss_with [ "alice", "bob" ]

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, issue: gitea.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Talking to a run in progress.** Everything above happens *between* runs: the brief is a
snapshot taken at claim time, so a comment posted while the agent is working is invisible
to the run it is about — and on a repo whose `on_done` closes the issue, it is invisible for
good. `follow_comments <duration>` closes that window. With it set, afkd asks the plugin
for the claimed issue's comments on that interval for the extent of the run and hands any
new one to the agent **that is already working**, as another turn in the same conversation:

```conf
import "@afkd/gitea"

widgets :: service(gitea) {
  base_url        "https://git.example.com"
  repo            "acme/widgets"
  token           "REPLACE_ME"
  poll_interval   4m to 6m
  follow_comments 60s          // the agent hears you mid-run

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, issue: gitea.Issue) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

Every delivered message is **also appended to the run's `task.md`**, so the next agent of a
multi-agent run reads the correction in the task itself, and a retry keeps it. The agent
picks a delivered message up at its next tool boundary. Comments are delivered once each,
oldest-first, attributed exactly as a brief's `## New comments` section is; afkd's own
comments and `[afkd-claim]` markers are never delivered back. A comment that lands after
the last tick is picked up by the **run-end sweep** afkd makes before the post-run slot,
and earns the issue another round of the same fire. Cost: one extra comment read per
interval **per running issue**, plus one at the end of each run, and nothing at all while
no run is in flight. All of this is afkd's own mid-run watch; the plugin only reads the
thread for it.

## `service(gitea.pr)` (pull-request review)

Fires on open pull requests — optionally only the bot's own — and re-fires when a human
leaves feedback newer than the bot's last word, for an automated review loop.

| Key               | Type       | Notes                                                  |
|-------------------|------------|--------------------------------------------------------|
| `base_url`        | `string`   | the Gitea instance base URL                            |
| `repo`            | `string`   | a single repository, `"owner/name"`; **exactly one** of `repo`/`org` |
| `org`             | `string`   | every repository of an org; **exactly one** of `repo`/`org` |
| `token`           | `string`   | personal access token; required                        |
| `author_me`       | `bool`     | restrict to the bot's own PRs; default `false`         |
| `follow_comments` | `duration` | as for `service(gitea)`                                |
| `max_attempts`    | `int`      | retries per round; default `1`                         |
| `poll_interval`   | `duration` | default `30s`; a range `2m to 3m` jitters              |

`source_label` and `discuss_with` are not keys here, and the kind has no `on_park` slot.

**Cadence.** Polls the forge every `poll_interval` for open PRs, optionally narrowed to the
bot's own via `author_me`, across a single `repo` or every repo of an `org`. Concurrency,
retries, and the slots behave as for `service(gitea)`.

**When a PR fires.** A PR is eligible when it carries **feedback newer than the bot's last
word**: the newest of the bot's own comments (by when they were last edited) and reviews
(by when they were submitted) is the watermark, and any other author's comment or review
after it is new feedback. A PR the bot has never spoken on counts all of its feedback as
new. Claim markers never count, the bot's own or a rival's. It is the agent's reply,
posted through the skill, that answers a round; until it does, the same feedback fires the
PR again on a later poll. The loop ends when a human merges or closes the PR, which drops
it from the open set — so there is no `gitea.pr_close(pr)` to call in `on_done`.

**The claim.** The same `[afkd-claim]` marker as `service(gitea)`, kept alive while the run
is and taken off the thread when it ends. The plugin adds `afkd/claimed` when it wins a PR,
and creates that label in the repository if it is missing (plain, never exclusive, as for
`service(gitea)`) — but here it is only **status**: the watermark, not the label, decides
whether a PR is claimed again, so the label stays on the PR between rounds and nothing
needs to remove it. There is no park and no clarification gate: a round either finishes
(`on_done`) or fails (`on_fail`).

**The brief.** A run's `task.md` names the PR and carries the new feedback, oldest first,
each item attributed to its author; a review appears as its id, for the agent to read with
the skill:

```markdown
Address review feedback on PR #7.

## New feedback

**陳大文:** the backoff never caps — see `retry.rs`

**carol:** (review 1042)
```

`follow_comments` works as for `service(gitea)`: afkd asks the plugin for the PR's comments
while a round runs, and the comments the brief was built from are never delivered again.

```conf
import "@afkd/gitea"

reviews :: service(gitea.pr) {
  base_url  "https://gitea.example.com"
  repo      "acme/widgets"
  token     "REPLACE_ME"
  author_me true

  on_claim(run: afkd.Run, pr: gitea.Pull_Request) { gitea.pr_assign_me(pr) }
  on_done(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome) { gitea.pr_unassign(pr) }

  work_dir "/srv/acme/widgets"
  on_run(run: afkd.Run, pr: gitea.Pull_Request) {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

## Slots, handles and actions

Both kinds' slots are code ([lang-v2 §12.6](https://afkd.sh/docs/lang-v2/)), each passed
the run, the claimed item, and after the run its outcome — by position, and a slot writes
every one of them, in order, each with its type:
`on_claim(run: afkd.Run, issue: gitea.Issue)` on `service(gitea)`, and
`on_done(run: afkd.Run, pr: gitea.Pull_Request, outcome: afkd.Outcome)` on `service(gitea.pr)`.

| Slot       | Passed               | `service(gitea)`                   | `service(gitea.pr)`             |
|------------|----------------------|------------------------------------|---------------------------------|
| `on_run`   | run, item            | the run itself                     | the run itself                  |
| `on_claim` | run, item            | once the issue is claimed, before the run | once the PR is claimed, before the run |
| `on_done`  | run, item, outcome   | after a run that finished          | after a round that finished     |
| `on_park`  | run, item, outcome   | after a run that ended waiting on a human reply | —                  |
| `on_fail`  | run, item, outcome   | after a run that failed            | after a round that failed       |

`run` is an `afkd.Run` (`id`, `scratch_dir`) and `outcome` an `afkd.Outcome` (`ok`,
`duration`, `error`). The item is the plugin's handle for it — a **`gitea.Issue`** on
`service(gitea)`, written `issue` in the examples here, and a **`gitea.Pull_Request`** on
`service(gitea.pr)`, written `pr`:

| Field    | Type           | `gitea.Issue` | `gitea.Pull_Request` | What it is                     |
|----------|----------------|:-------------:|:--------------------:|--------------------------------|
| `id`     | `string`       | ✓             | ✓                    | the number, `"7"`              |
| `key`    | `string`       | ✓             | ✓                    | afkd's claim key for it        |
| `title`  | `string`       | ✓             | ✓                    | its title                      |
| `url`    | `string`       | ✓             | ✓                    | its page on the forge          |
| `number` | `int`          | ✓             | ✓                    | its number in the repository   |
| `labels` | `list[string]` | ✓             |                      | the names of its labels        |
| `branch` | `string`       |               | ✓                    | the PR's head branch           |

A config passes an item around and compares it, but never builds one. The plugin sends every
field with the claim, as the claiming poll read the item, and they hold for the whole run:
`labels` shows the issue's labels before the claim's own changes, so never the
`afkd/claimed` the claim adds.

afkd runs a slot's statements in the order they are written, and each action it calls is
one request to the plugin, which does it on the item the action is passed first, through
Gitea's native primitives. A config's types have no subtyping, so each kind has its own six:
`service(gitea)`'s take a `gitea.Issue`, and `service(gitea.pr)`'s — the same six, named
with a `pr_` in front — a `gitea.Pull_Request`:

| Action (`service(gitea)`)             | Action (`service(gitea.pr)`)          | Effect                                  |
|---------------------------------------|---------------------------------------|-----------------------------------------|
| `gitea.assign_me(issue)`              | `gitea.pr_assign_me(pr)`              | add the bot to the assignees (a human's stay) |
| `gitea.unassign(issue)`               | `gitea.pr_unassign(pr)`               | remove the bot from the assignees (a human's stay) |
| `gitea.label_add(issue, "<name>")`    | `gitea.pr_label_add(pr, "<name>")`    | add a named label; the repository must define it |
| `gitea.label_remove(issue, "<name>")` | `gitea.pr_label_remove(pr, "<name>")` | remove a named label; a name the repository does not define has nothing to remove |
| `gitea.close(issue)`                  | `gitea.pr_close(pr)`                  | close the issue or PR                   |
| `gitea.comment(issue, "<text>")`      | `gitea.pr_comment(pr, "<text>")`      | post a comment, verbatim; in a post-run slot, `#{outcome.duration}` interpolates how long the run took |

Passing the other kind's item — `gitea.comment(pr, "…")` in a `service(gitea.pr)` — or none
at all is refused when the config loads.

**`gitea.me`** is the login the `token` authenticates as — `autocoder`, say — for a slot to
name the bot in a comment or a label. The plugin reads it off the forge when the service
starts.

Each action returns a result, like any call: one the forge refused — a label the repository
does not define (Gitea silently ignores such a name rather than failing, and the plugin
turns that into an error), a forge that is down — fails with the plugin's sentence. A
failing `on_claim` gives the issue back and fails the run; a failing post-run slot is logged
and changes nothing. afkd never retries a slot's action.

**Mind the spelling.** The label actions are written verb-last, `gitea.label_add` and
`gitea.label_remove`; `@afkd/trello` writes the same two verb-first, `trello.add_label` and
`trello.remove_label`.

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **afkd runs the slots.** The built-in ran its lifecycle blocks itself, inside its own
  claim and finish. Here a slot is code afkd runs in the order it is written, and each
  action is one request to the plugin naming the item it acts on, sent once the plugin's
  own claim or finish has landed: `on_claim` after the claim is won, and the post-run slot
  after the claim marker is released and, on the `discuss_with` path, after the backstop —
  so a comment `on_done` posts lands after `reviewed, nothing to add` rather than standing
  it down. afkd retries no slot's action, so a post-run
  `gitea.label_remove(issue, "afkd/claimed")` that fails leaves the re-pick gate on the
  issue for a human.
- **`gitea.me` is read when the service starts.** The plugin asks the forge who the token
  is when afkd greets it. If the forge cannot say then, the service starts anyway — the
  first poll asks again before it claims — but a slot that reads `gitea.me` fails until the
  plugin is restarted.
- **Some settings are refused when the service starts, not at `afkd validate`.** afkd types
  the settings against the plugin's manifest before the plugin ever runs. What it cannot
  see — both or neither of `repo`/`org`, a `discuss_with` that names nobody — the plugin
  refuses when it is greeted, in the built-in's own words, and the service ends there.
- **A park that did not land is released at once.** When the park's label swap cannot be
  carried out, the built-in holds the claim and releases it on its next poll. The plugin
  releases it straight away — drops `afkd/claimed` and the marker — so the next poll
  retries the issue. The second time the same issue fails that way it is left claimed, for
  a human, exactly as the built-in does; both say so in the service log.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole issue through the skill. A thread with more
  new comments than fit in one reply delivers the newest, and names the ones left out.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, so an org-wide scan against a slow forge stops after 20 and leaves the rest of the
  repositories for the next poll. And every call the plugin answers comes back within 45
  seconds: a forge too slow to answer in that time is treated as if it were down.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  unit acme/widgets#7`, a review round's too, where the built-in's named the vendor and the
  kind in place of `plugin unit` — and a comment delivered mid-run calls the issue or PR
  "this work item" rather than "this issue" or "this pull request".
