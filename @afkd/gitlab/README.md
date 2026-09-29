# `@afkd/gitlab`

A **provider** plugin with two trigger kinds. [`service(gitlab)`](#servicegitlab-issues)
turns open issues on a GitLab project into afkd runs, and gives the service's hooks the
actions that reflect progress back through each issue's assignees, labels and state.
[`service(gitlab.mr)`](#servicegitlabmr-merge-request-review) re-fires a run on a merge
request each time a human leaves a note newer than the bot's last word, for an automated
review loop. The plugin also ships the `gitlab` skill an agent uses to answer the issue or
merge request: read and post notes, fetch and post attachments, list the project's issues,
and open a merge request.

It is afkd's two built-in GitLab triggers, moved out of afkd: the same keys, the same claim
markers on the issue or merge request, the same claim-journal keys and session threads, the
same run environment and the same brief. A claim the built-in left on a live issue or merge
request is recognised, renewed and released by the plugin, and the other way round, so
switching from one to the other strands nothing. The few places the plugin behaves
differently are listed [at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks, and
[config language v2](https://afkd.sh/docs/lang-v2/) for the configs that use it.

It is a Rust program, built from source when it is installed.

## Install it

```console
$ afkd install @afkd/gitlab
```

From a checkout of this repository, name the directory instead:

```console
$ afkd install /path/to/afkd-plugins/@afkd/gitlab
```

afkd copies the tree and runs `cargo build --release --locked` in it, so the host needs a
Rust toolchain — the one afkd itself was installed with is enough. The first build fetches
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs).

Run it on **afkd 0.2.186 or newer**, the first to speak plugin protocol 2: this is a
manifest v2 plugin, which a [config language v2](https://afkd.sh/docs/lang-v2/) file
imports, and afkd runs its hooks, sending each action a hook calls as a `call`. An older
afkd refuses the plugin at its `hello`.

A config file uses the plugin by importing it, and then names it by its leaf, `gitlab`:

```conf
package main

import "@afkd/gitlab"

GITLAB_TOKEN :: env.GITLAB_TOKEN

task :: proc() {
  $ cat $AFKD_SCRATCH_DIR/task.md
}

develop :: service(gitlab) {
  base_url      "https://gitlab.example.com"
  project       "group/widgets"
  token         GITLAB_TOKEN
  source_label  "afkd::ready"
  poll_interval 1m to 3m

  on_claim {
    gitlab.assign_me()
    gitlab.label_add("afkd::working")
  }
  on_done {
    gitlab.label_remove("afkd::working")
    gitlab.comment("Fixed in #{run.duration} by #{gitlab.me}.")
    gitlab.close()
  }
  on_fail {
    gitlab.label_remove("afkd::working")
    gitlab.unassign()
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

// The agent a `service(gitlab)` runs: the skill is what lets it answer the issue.
fixer :: agent(claude) {
  model  "sonnet"
  skills [ "@afkd/gitlab/gitlab" ]
}
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`GITLAB_TOKEN`, `GITLAB_BASE_URL`, `GITLAB_PROJECT` and `GITLAB_ISSUE_NUMBER` every
`service(gitlab)` run carries — `GITLAB_MR_NUMBER` and `GITLAB_MR_BRANCH` in place of the
issue number on a `service(gitlab.mr)` run — and falls back to the bare number under
`issue/number` or `mr/number` in the run's scratch directory.

## `service(gitlab)` (issues)

Fires for open issues on a single GitLab project, and its hooks reflect progress back
through the issue's assignees, labels, and state. Its token stays with the plugin and the
run.

| Key               | Type       | Notes                                              |
|-------------------|------------|----------------------------------------------------|
| `base_url`        | `string`   | optional; empty → `https://gitlab.com` (API under `/api/v4`) |
| `project`         | `string`   | **required**: a numeric id or a path-with-namespace (`group/widgets`) |
| `token`           | `string`   | personal/project access token (`PRIVATE-TOKEN`); required |
| `source_label`    | `string`   | optional; restrict to issues carrying this label   |
| `follow_comments` | `duration` | re-read the issue this often **while its run is in flight**, delivering new notes to the working agent; a range `30s to 90s` jitters; default unset (no mid-run watch) |
| `max_attempts`    | `int`      | retries per issue; default `1`                     |
| `poll_interval`   | `duration` | default `30s`; a range `2m to 3m` jitters          |

The last three are afkd's own, read by afkd for every kind that claims its work.

There is no `group`/org key — a GitLab trigger polls a single `project` (required,
non-empty). A path-with-namespace `project` is URL-encoded to its `:id`
(`group/widgets` → `group%2Fwidgets`); a numeric id passes through. With `source_label`
unset every open issue of the project is up for grabs; set, only the issues carrying it.

The hooks only manage the issue's *status* — its assignees, labels, and state — through one
`PUT` per action. The **claim** itself is a `[afkd-claim]` marker note the plugin posts and
releases on its own; no hook holds or releases it, and an issue a human is assigned to is
still claimable (a person is not a competing claimant). afkd keeps that marker **alive
while the run is**, asking the plugin to edit it every few minutes, so a run past an hour
still holds its issue and a second instance loses the race rather than double-claiming it;
only a marker nobody is renewing any more ages out.

**The claim label.** The plugin holds its re-pick gate in `afkd::claimed`: it adds the
label itself when a claim is won, and an issue carrying it is never picked up again (a
human, or a hook's `gitlab.label_remove("afkd::claimed")`, removes it to retry). GitLab
creates a label the first time it is used, so there is nothing to define up front.

**Cadence.** Polls the forge every `poll_interval` for open issues carrying `source_label`
(if set); a service works **one issue at a time, to completion**, and `max_attempts` bounds
the per-issue retries. There is no clarification gate and no `on_park`.

**Saying what happened.** A `gitlab.comment(…)` in `on_done`/`on_fail` can carry the run's
own facts, which afkd interpolates before the plugin sees the text: `#{run.duration}`,
`#{run.cost}` and `#{run.turns}`.

```conf
package main

import "@afkd/gitlab"

widgets :: service(gitlab) {
  project "4242"
  token   "REPLACE_ME"

  on_done {
    gitlab.comment("Fixed in #{run.duration} for #{run.cost}.")
    gitlab.close()
  }
  on_fail {
    gitlab.comment("Gave up after #{run.turns} turns.")
    gitlab.unassign()
  }

  work_dir "/srv/acme/widgets"
  on_run {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

**Talking to a run in progress.** The brief is a snapshot taken at claim time, so a note
posted while the agent is working is invisible to the run it is about — and on a project
whose `on_done` closes the issue, it is invisible for good. `follow_comments <duration>`
closes that window. With it set, afkd asks the plugin for the claimed issue's notes on that
interval for the extent of the run and hands any new one to the agent **that is already
working**, as another turn in the same conversation:

```conf
package main

import "@afkd/gitlab"

widgets :: service(gitlab) {
  base_url        "https://gitlab.example.com"
  project         "group/sub.group/widgets"
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
multi-agent run reads the correction in the task itself, and a retry keeps it. Notes are
delivered once each, oldest-first; afkd's own notes and `[afkd-claim]` markers are never
delivered back. The issue claim reads no notes, so the first read takes the thread as it
stands as the baseline and delivers only what comes after. Cost: one extra notes read per
interval **per running issue**, plus one at the end of each run, and nothing at all while
no run is in flight. All of this is afkd's own mid-run watch; the plugin only reads the
thread for it.

## `service(gitlab.mr)` (merge-request review)

Fires on open merge requests — optionally only the bot's own — and re-fires when a human
leaves a note newer than the bot's last word, for an automated review loop.

| Key               | Type       | Notes                                              |
|-------------------|------------|----------------------------------------------------|
| `base_url`        | `string`   | as for `service(gitlab)`                           |
| `project`         | `string`   | **required**: a numeric id or a path-with-namespace |
| `token`           | `string`   | personal/project access token; required           |
| `author_me`       | `bool`     | restrict to the bot's own MRs; default `false`     |
| `follow_comments` | `duration` | as for `service(gitlab)`                           |
| `max_attempts`    | `int`      | retries per round; default `1`                     |
| `poll_interval`   | `duration` | default `30s`; a range `2m to 3m` jitters          |

`source_label` is **not** a key here.

**Cadence.** Polls the forge every `poll_interval` for the project's open merge requests,
optionally narrowed to the bot's own via `author_me`. Concurrency, retries, and the hooks
behave as for `service(gitlab)`.

**When an MR fires.** Feedback is read from the MR's **notes** — GitLab has no separate
review list. An MR is eligible when it carries **a note newer than the bot's last word**:
the newest of the bot's own notes, by when it was last edited, is the watermark, and any
other author's note touched after it is new feedback. An MR the bot has never spoken on
counts all of its notes as new. Claim markers never count, the bot's own or a rival's. It
is the agent's reply, posted through the skill, that answers a round; until it does, the
same feedback fires the MR again on a later poll. A `gitlab.comment(…)` in `on_done` or
`on_fail` is the bot speaking too, so it answers the round as well. The loop ends when a
human merges or closes the MR, which drops it from the open set — so there is no
`gitlab.close()` to call in `on_done`.

**The claim.** The same `[afkd-claim]` marker as `service(gitlab)`, kept alive while the
run is and taken off the thread when it ends. The plugin adds `afkd::claimed` when it wins
an MR, but here it is only **status**: the watermark, not the label, decides whether an MR
is claimed again, so the label stays on the MR between rounds and nothing needs to remove
it. There is no park and no clarification gate: a round either finishes (`on_done`) or
fails (`on_fail`).

**The brief.** A run's `task.md` names the MR and carries the new feedback, oldest first,
each note attributed to its author:

```markdown
Address review feedback on MR !7.

## New feedback

**陳大文:** the backoff never caps — see `retry.rs`

**carol:** and the jitter is still zero
```

`follow_comments` works as for `service(gitlab)`: afkd asks the plugin for the MR's notes
while a round runs. The notes the brief was built from are never delivered again.

```conf
package main

import "@afkd/gitlab"

reviews :: service(gitlab.mr) {
  base_url        "https://gitlab.example.com"
  project         "group/widgets"
  token           "REPLACE_ME"
  author_me       true
  follow_comments 60s

  on_claim {
    gitlab.assign_me()
    gitlab.label_add("afkd::reviewing")
  }
  on_done { gitlab.label_remove("afkd::reviewing") }
  on_fail {
    gitlab.label_remove("afkd::reviewing")
    gitlab.unassign()
  }

  work_dir "/srv/acme/widgets"
  on_run {
    $ cat $AFKD_SCRATCH_DIR/task.md
  }
}
```

## Hooks and actions

Both kinds' hooks are code slots ([lang-v2 §13.8](https://afkd.sh/docs/lang-v2/)):
`on_claim` runs once an issue or merge request is claimed, before the run; `on_done` and
`on_fail` after the run, the one its outcome picks. afkd runs a hook's statements in the
order they are written, and each action it calls is one request to the plugin, which does
it on the claimed issue or merge request. GitLab's assignee write replaces the whole set,
so `assign_me` and `unassign` read it first and write it back changed by one row — neither
touches an assignment afkd did not make:

| Action                          | Effect                                          |
|---------------------------------|-------------------------------------------------|
| `gitlab.assign_me()`            | add the bot to the assignees (`assignee_ids`, unioned in) |
| `gitlab.unassign()`             | remove **only** the bot from the assignees (a human's stays) |
| `gitlab.label_add("<name>")`    | add a named label (`add_labels`)                |
| `gitlab.label_remove("<name>")` | remove a named label (`remove_labels`, by name) |
| `gitlab.close()`                | close the issue (`state_event=close`)           |
| `gitlab.comment("<text>")`      | post a note, verbatim; in a post-run hook, `#{run.duration}`, `#{run.cost}` and `#{run.turns}` interpolate the run's facts (afkd rejects them in `on_claim`) |

**`gitlab.me`** is the username the `token` authenticates as — `autocoder`, say — for a
hook to name the bot in a note or a label. The plugin reads it off the forge when the
service starts.

Each action returns a result, like any call: one the forge refused — a forge that is down,
a token without the scope — fails with the plugin's sentence. A failing `on_claim` gives
the issue back and fails the run; a failing post-run hook is logged and changes nothing.
afkd never retries a hook action.

**Mind the spelling.** The label actions are written verb-last, `gitlab.label_add` and
`gitlab.label_remove`; `@afkd/trello` writes the same two verb-first, `trello.add_label`
and `trello.remove_label`.

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **afkd runs the hooks.** The built-in ran its lifecycle blocks itself, inside its own
  claim and finish. Here a hook is code afkd runs in the order it is written, and each
  action is one request to the plugin, sent once the plugin's own claim or finish has
  landed: `on_claim` after the claim is won, and the post-run hook after the claim marker
  is released. afkd retries no hook action, so a post-run
  `gitlab.label_remove("afkd::claimed")` that fails leaves the re-pick gate on the issue
  for a human.
- **`gitlab.me` is read when the service starts.** The plugin asks GitLab who the token is
  when afkd greets it. If GitLab cannot say then, the service starts anyway — the first
  call that needs the identity asks again — but a hook that reads `gitlab.me` fails until
  the plugin is restarted.
- **Some settings are refused when the service starts, not at `afkd validate`.** afkd types
  the settings against the plugin's manifest before the plugin ever runs. What it cannot
  see — an empty `project` — the plugin refuses when it is greeted, in the built-in's own
  words, and the service ends there.
- **The identity is looked up by whichever call needs it first.** A release unassigns the
  bot by its user id, and afkd may ask for a release before it ever polls — after a
  restart, for a claim a crashed run left. If GitLab would not say who the token belongs to
  at `hello`, the release asks again; if it still cannot, the plugin writes nothing and
  afkd keeps the claim to ask again, as the built-in's reaper waits for the identity.
- **No stop mid-claim.** The built-in abandons a claim a shutdown lands in the middle of;
  the plugin cannot see afkd stop, so it finishes the claim, and afkd hands the unit
  straight back with a release. The issue or merge request ends where the built-in leaves
  it.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole issue through the skill — the words are the
  same on a merge request's brief, and the skill reads the MR's notes there. A thread with
  more new notes than fit in one reply delivers the newest, and names the ones left out.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, and each claim attempt waits a second for a rival's marker to show, so a scan that
  keeps losing races stops after 20 and leaves the rest of the issues or merge requests
  for the next poll. And every call the plugin answers comes back within 45 seconds: a
  forge too slow to answer in that time is treated as if it were down.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  unit group/widgets#7`, a review round's too, where the built-in's named the vendor and
  the kind in place of `plugin unit` — and a note delivered mid-run calls the issue or
  merge request "this work item" rather than "this issue" or "this merge request".
