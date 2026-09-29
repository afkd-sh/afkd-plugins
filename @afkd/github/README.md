# `@afkd/github`

A **provider** plugin with two trigger kinds. [`service(github)`](#servicegithub-issues)
turns open issues on a GitHub repository into afkd runs, and gives the service's hooks the
actions that reflect progress back through each issue's assignees, labels and state.
[`service(github.pr)`](#servicegithubpr-pull-request-review) re-fires a run on a pull
request each time a human leaves a comment or review newer than the bot's last word, for an
automated review loop. The plugin also ships the `github` skill an agent uses to answer the
issue or pull request: read and post comments, fetch the images pasted into it, and open a
pull request for the branch it committed.

It is afkd's two built-in GitHub triggers, moved out of afkd: the same keys, the same claim
markers on the issue or pull request, the same claim-journal keys and session threads, the
same run environment and the same brief. A claim the built-in left on a live issue or pull
request is recognised, renewed and released by the plugin, and the other way round, so
switching from one to the other strands nothing. The few places the plugin behaves
differently are listed [at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[config language v2](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, built from source when it is installed.

## Install it

```console
$ afkd install @afkd/github
```

From a checkout of this repository, name the directory instead:

```console
$ afkd install /path/to/afkd-plugins/@afkd/github
```

afkd copies the tree and runs `cargo build --release --locked` in it, so the host needs a
Rust toolchain — the one afkd itself was installed with is enough. The first build fetches
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs).

Run it on **afkd 0.2.186 or newer**, the first to speak plugin protocol 2: this is a
manifest v2 plugin, which a [config language v2](https://afkd.sh/docs/lang-v2/) file
imports, and afkd runs its hooks, sending each action a hook calls as a `call`. An older
afkd refuses the plugin at its `hello`.

A config file uses the plugin by importing it, and then names it by its leaf, `github`:

```conf
package main

import "@afkd/github"

GITHUB_TOKEN :: env.GITHUB_TOKEN

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task.md
}

develop :: service(github) {
  repo          "acme/widgets"
  token         GITHUB_TOKEN
  source_label  "afkd/ready"
  poll_interval 1m to 3m

  on_claim {
    github.assign_me()
    github.label_add("afkd/working")
  }
  on_done {
    github.label_remove("afkd/working")
    github.comment("Fixed in #{run.duration} by #{github.me}.")
    github.close()
  }
  on_fail {
    github.label_remove("afkd/working")
    github.unassign()
  }

  work_dir "/srv/acme/widgets"
  on_run { task() }
}
```

The token comes from the daemon's environment, so it never sits in the file.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
package main

// The agent a `service(github)` runs: the skill is what lets it answer the issue.
fixer :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/github/github" ]
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`GITHUB_TOKEN`, `GITHUB_HOST`, `GITHUB_REPO` and `GITHUB_ISSUE_NUMBER` every
`service(github)` run carries, and falls back to the bare issue number under
`issue/number` in the run's scratch directory. A `service(github.pr)` run carries
`GITHUB_PR_NUMBER` and `GITHUB_PR_BRANCH` (the PR's head branch) in place of the issue
number, and the bare number under `pr/number`; the skill reads and posts on the pull
request's conversation the same way.

## `service(github)` (issues)

Fires for open issues on a single GitHub repository, and its hooks reflect progress back
through the issue's assignees, labels, and state. Its token stays with the plugin and the
run.

| Key               | Type       | Notes                                              |
|-------------------|------------|----------------------------------------------------|
| `host`            | `string`   | optional; empty/`github.com` → the cloud API, any other host → GitHub Enterprise `…/api/v3` |
| `repo`            | `string`   | **required**: a single repository, `"owner/name"`  |
| `token`           | `string`   | personal access token (`Authorization: Bearer`); required |
| `source_label`    | `string`   | optional; restrict to issues carrying this label   |
| `follow_comments` | `duration` | re-read the issue this often **while its run is in flight**, delivering new comments to the working agent; a range `30s to 90s` jitters; default unset (no mid-run watch) |
| `max_attempts`    | `int`      | retries per issue; default `1`                     |
| `poll_interval`   | `duration` | default `30s`; a range `2m to 3m` jitters          |

The last three are afkd's own, read by afkd for every kind that claims its work.

There is no `org` key — a GitHub trigger polls a single `repo` (required, non-empty). A
`repo` that is not `owner/name` claims nothing. `author_me` is **not** a key here; it
belongs to [`service(github.pr)`](#servicegithubpr-pull-request-review). GitHub lists pull
requests among a repo's issues; the plugin passes them over, so a pull request carrying the
source label is never claimed as an issue. With `source_label` unset every open issue of
the repo is up for grabs; set, only the issues carrying it.

The hooks only manage the issue's *status* — its assignees, labels, and state. The
**claim** itself is a `[afkd-claim]` marker comment the plugin posts and releases on its
own; no hook holds or releases it, and an issue a human is assigned to is still claimable
(a person is not a competing claimant). afkd keeps that marker **alive while the run is**,
asking the plugin to edit it every few minutes, so a run past an hour still holds its issue
and a second instance loses the race rather than double-claiming it; only a marker nobody is
renewing any more ages out.

**The claim label.** The plugin holds its re-pick gate in `afkd/claimed`: it adds the label
itself when a claim is won, and an issue carrying it is never picked up again (a human, or
a hook's `github.label_remove("afkd/claimed")`, removes it to retry). GitHub creates a label
the first time it is added to an issue, so there is nothing to define up front.

**Cadence.** Polls the forge every `poll_interval` for open issues carrying `source_label`
(if set); a service works **one issue at a time, to completion**, and `max_attempts` bounds
the per-issue retries. There is no clarification gate and no `on_park`.

**Saying what happened.** A `github.comment(…)` in `on_done`/`on_fail` can carry the run's
own facts, which afkd interpolates before the plugin sees the text: `#{run.duration}`,
`#{run.cost}` and `#{run.turns}`.

```conf
package main

import "@afkd/github"

widgets :: service(github) {
  host  "ghe.example.com"
  repo  "acme/widgets"
  token "REPLACE_ME"

  on_done {
    github.comment("Fixed in #{run.duration} for #{run.cost}.")
    github.close()
  }
  on_fail {
    github.comment("Gave up after #{run.turns} turns.")
    github.unassign()
  }

  work_dir "/srv/acme/widgets"
  on_run {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Talking to a run in progress.** The brief is a snapshot taken at claim time, so a comment
posted while the agent is working is invisible to the run it is about — and on a repo whose
`on_done` closes the issue, it is invisible for good. `follow_comments <duration>` closes
that window. With it set, afkd asks the plugin for the claimed issue's comments on that
interval for the extent of the run and hands any new one to the agent **that is already
working**, as another turn in the same conversation:

```conf
package main

import "@afkd/github"

widgets :: service(github) {
  repo            "acme/widgets"
  token           "REPLACE_ME"
  poll_interval   4m to 6m
  follow_comments 60s          // the agent hears you mid-run

  work_dir "/srv/acme/widgets"
  on_run {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

Every delivered message is **also appended to the run's `task.md`**, so the next agent of a
multi-agent run reads the correction in the task itself, and a retry keeps it. Comments are
delivered once each, oldest-first; afkd's own comments and `[afkd-claim]` markers are never
delivered back. The issue claim reads no comments, so the first read takes the thread as it
stands as the baseline and delivers only what comes after. Cost: one extra comments read
per interval **per running issue**, plus one at the end of each run, and nothing at all
while no run is in flight. All of this is afkd's own mid-run watch; the plugin only reads
the thread for it.

## `service(github.pr)` (pull-request review)

Fires on open pull requests — optionally only the bot's own — and re-fires when a human
leaves feedback newer than the bot's last word, for an automated review loop.

| Key               | Type       | Notes                                              |
|-------------------|------------|----------------------------------------------------|
| `host`            | `string`   | as for `service(github)`                           |
| `repo`            | `string`   | **required**: a single repository, `"owner/name"`  |
| `token`           | `string`   | personal access token; required                    |
| `author_me`       | `bool`     | restrict to the bot's own PRs; default `false`     |
| `follow_comments` | `duration` | as for `service(github)`                           |
| `max_attempts`    | `int`      | retries per round; default `1`                     |
| `poll_interval`   | `duration` | default `30s`; a range `2m to 3m` jitters          |

`source_label` is **not** a key here.

**Cadence.** Polls the forge every `poll_interval` for the repo's open pull requests,
optionally narrowed to the bot's own via `author_me`. Concurrency, retries, and the hooks
behave as for `service(github)`.

**When a PR fires.** Feedback is read from the PR's conversation **comments** and its
**reviews**. A PR is eligible when it carries **feedback newer than the bot's last word**:
the newest of the bot's own comments (by when it was last edited) and reviews (by when it
was submitted) is the watermark, and any other author's comment touched after it, or review
submitted after it, is new feedback. A PR the bot has never spoken on counts all of it as
new. Claim markers never count, the bot's own or a rival's. It is the agent's reply,
posted through the skill, that answers a round; until it does, the same feedback fires the
PR again on a later poll. A `github.comment(…)` in `on_done` or `on_fail` is the bot
speaking too, so it answers the round as well. The loop ends when a human merges or closes
the PR, which drops it from the open set — so there is no `github.close()` to call in
`on_done`.

**The claim.** The same `[afkd-claim]` marker as `service(github)` — GitHub treats a pull
request as an issue, so the marker, the labels and the assignees go on the PR's own
conversation — kept alive while the run is and taken off the thread when it ends. The
plugin adds `afkd/claimed` when it wins a PR, but here it is only **status**: the
watermark, not the label, decides whether a PR is claimed again, so the label stays on the
PR between rounds and nothing needs to remove it. There is no park and no clarification
gate: a round either finishes (`on_done`) or fails (`on_fail`).

**The brief.** A run's `task.md` names the PR and carries the new feedback, oldest first,
each item attributed to its author; a review stands as its id:

```markdown
Address review feedback on PR #7.

## New feedback

**陳大文:** the backoff never caps — see `retry.rs`

**carol:** (review 2291)
```

`follow_comments` works as for `service(github)`: afkd asks the plugin for the PR's
comments while a round runs. The comments the brief was built from are never delivered
again.

```conf
package main

import "@afkd/github"

reviews :: service(github.pr) {
  host            "github.com"
  repo            "acme/widgets"
  token           "REPLACE_ME"
  author_me       true
  follow_comments 60s

  on_claim {
    github.assign_me()
    github.label_add("afkd/reviewing")
  }
  on_done { github.label_remove("afkd/reviewing") }
  on_fail {
    github.label_remove("afkd/reviewing")
    github.unassign()
  }

  work_dir "/srv/acme/widgets"
  on_run {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

## Hooks and actions

Both kinds' hooks are code slots ([lang-v2 §13.8](https://afkd.sh/docs/lang-v2/)):
`on_claim` runs once an issue or pull request is claimed, before the run; `on_done` and
`on_fail` after the run, the one its outcome picks. afkd runs a hook's statements in the
order they are written, and each action it calls is one request to the plugin, which does
it on the claimed issue or pull request. GitHub assignment is additive, so `assign_me` and
`unassign` add and remove only the bot; label removal is by name (never GitHub's
all-clearing `…/labels` path):

| Action                          | Effect                                          |
|---------------------------------|-------------------------------------------------|
| `github.assign_me()`            | add the bot to the assignees                    |
| `github.unassign()`             | remove **only** the bot from the assignees (a human's stays) |
| `github.label_add("<name>")`    | add a named label                               |
| `github.label_remove("<name>")` | remove a named label (by name)                  |
| `github.close()`                | close the issue (`state=closed`)                |
| `github.comment("<text>")`      | post a comment, verbatim; in a post-run hook, `#{run.duration}`, `#{run.cost}` and `#{run.turns}` interpolate the run's facts (afkd rejects them in `on_claim`) |

**`github.me`** is the login the `token` authenticates as — `autocoder[bot]`, say — for a
hook to name the bot in a comment or a label. The plugin reads it off the forge when the
service starts.

Each action returns a result, like any call: one the forge refused — a forge that is down,
a token without the scope — fails with the plugin's sentence. A failing `on_claim` gives
the issue back and fails the run; a failing post-run hook is logged and changes nothing.
afkd never retries a hook action.

**Mind the spelling.** The label actions are written verb-last, `github.label_add` and
`github.label_remove`; `@afkd/trello` writes the same two verb-first, `trello.add_label`
and `trello.remove_label`.

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **afkd runs the hooks.** The built-in ran its lifecycle blocks itself, inside its own
  claim and finish. Here a hook is code afkd runs in the order it is written, and each
  action is one request to the plugin, sent once the plugin's own claim or finish has
  landed: `on_claim` after the claim is won, and the post-run hook after the claim marker
  is released. afkd retries no hook action, so a post-run
  `github.label_remove("afkd/claimed")` that fails leaves the re-pick gate on the issue for
  a human.
- **`github.me` is read when the service starts.** The plugin asks GitHub who the token is
  when afkd greets it. If GitHub cannot say then, the service starts anyway — the first
  call that needs the identity asks again — but a hook that reads `github.me` fails until
  the plugin is restarted.
- **Some settings are refused when the service starts, not at `afkd validate`.** afkd types
  the settings against the plugin's manifest before the plugin ever runs. What it cannot
  see — an empty `repo` — the plugin refuses when it is greeted, in the built-in's own
  words, and the service ends there.
- **The identity is looked up by whichever call needs it first.** A release unassigns the
  bot by its login, and afkd may ask for a release before it ever polls — after a restart,
  for a claim a crashed run left. If GitHub would not say who the token belongs to at
  `hello`, the release asks again; if it still cannot, the plugin writes nothing and afkd
  keeps the claim to ask again, as the built-in's reaper waits for the identity.
- **No stop mid-claim.** The built-in abandons a claim a shutdown lands in the middle of;
  the plugin cannot see afkd stop, so it finishes the claim, and afkd hands the unit
  straight back with a release. The issue or pull request ends where the built-in leaves
  it.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole issue through the skill — the words are the
  same on a pull request's brief, and the skill reads the PR's conversation there. A
  thread with more new comments than fit in one reply delivers the newest, and names the
  ones left out.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, and each claim attempt waits a second for a rival's marker to show, so a scan that
  keeps losing races stops after 20 and leaves the rest of the issues or pull requests for
  the next poll. And every call the plugin answers comes back within 45 seconds: a forge
  too slow to answer in that time is treated as if it were down.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  unit acme/widgets#7`, a review round's too, where the built-in's named the vendor and the
  kind in place of `plugin unit` — and a comment delivered mid-run calls the issue or pull
  request "this work item" rather than "this issue" or "this pull request".
