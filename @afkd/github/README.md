# `@afkd/github`

A **provider** plugin with one trigger kind, [`github`](#github-issues): it turns open
issues on a GitHub repository into afkd runs, and reflects progress back through each
issue's assignees, labels and state. It also ships the `github` skill an agent uses to
answer the issue: read and post comments, fetch the images pasted into it, and open a pull
request for the branch it committed.

It is afkd's built-in GitHub issue trigger, moved out of afkd: the same keys, the same
claim markers and lifecycle comments on the issue, the same claim-journal keys and session
threads, the same run environment and the same brief. A claim the built-in left on a live
issue is recognised, renewed and released by the plugin, and the other way round, so
switching from one to the other strands nothing. The few places the plugin behaves
differently are listed [at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks. The built-in's
second GitHub kind, `github_pr_review`, is not part of this plugin yet.

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
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs). An afkd that
still has `github` compiled in refuses the install, because a plugin may not shadow a
built-in kind; use the built-in there, with exactly the same config.

Run it on an afkd that understands a `held` finish (see
[below](#where-it-differs-from-the-built-in)); an older one treats an undelivered
`on_done`/`on_fail` as delivered and never retries it.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
# The agent a `github` service runs: the skill is what lets it answer the issue.
agent fixer { worker claude { model sonnet; skills @afkd/github/github } }
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`GITHUB_TOKEN`, `GITHUB_HOST`, `GITHUB_REPO` and `GITHUB_ISSUE_NUMBER` every `github` run
carries, and falls back to the bare issue number under `issue/number` in the run's scratch
directory.

## `github` (issues)

Fires for open issues on a single GitHub repository and reflects progress back through the
issue's assignees, labels, and state. Its token stays with the plugin and the run.

| Key               | Value            | Notes                                              |
|-------------------|------------------|----------------------------------------------------|
| `host`            | `"<host>"`       | optional; empty/`github.com` → the cloud API, any other host → GitHub Enterprise `…/api/v3` |
| `repo`            | `"owner/name"`   | **required**: a single repository                  |
| `token`           | `"<token>"`      | personal access token (`Authorization: Bearer`); required |
| `source_label`    | `"<label>"`      | optional; restrict to issues carrying this label   |
| `follow_comments` | `<duration>`     | re-read the issue this often **while its run is in flight**, delivering new comments to the working agent; a range jitters (see [Durations](https://afkd.sh/docs/configuration/#durations)); default unset (no mid-run watch) |
| `max_attempts`    | `<n>`            | retries per issue; default `1`                      |
| `poll_interval`   | `<duration>`     | default `30s`; a range jitters (see [Durations](https://afkd.sh/docs/configuration/#durations)) |
| `on_claim`        | `{ <actions> }`  | issue-lifecycle actions when work starts           |
| `on_done`         | `{ <actions> }`  | issue-lifecycle actions when work finishes         |
| `on_fail`         | `{ <actions> }`  | issue-lifecycle actions when work fails            |

There is no `org` key — a GitHub trigger polls a single `repo` (required, non-empty). A
`repo` that is not `owner/name` claims nothing. `author_me` is **not** a key here; it
belongs to the built-in's `github_pr_review`. GitHub lists pull requests among a repo's
issues; the plugin passes them over, so a pull request carrying the source label is never
claimed as an issue. With `source_label` unset every open issue of the repo is up for
grabs; set, only the issues carrying it.

The lifecycle actions only manage the issue's *status* — its assignees, labels, and state.
The **claim** itself is a `[afkd-claim]` marker comment the plugin posts and releases on
its own; nothing here holds or releases it, and an issue a human is assigned to is still
claimable (a person is not a competing claimant). afkd keeps that marker **alive while the
run is**, asking the plugin to edit it every few minutes, so a run past an hour still holds
its issue and a second instance loses the race rather than double-claiming it; only a
marker nobody is renewing any more ages out.

**The claim label.** The plugin holds its re-pick gate in `afkd/claimed`: it adds the label
itself when a claim is won, and an issue carrying it is never picked up again (a human
removes it to retry). GitHub creates a label the first time it is added to an issue, so
there is nothing to define up front.

GitHub assignment is additive, so `assign_me`/`unassign` add/remove only the bot; label
removal is by name (never GitHub's all-clearing `…/labels` path):

| Action                  | Effect                                          |
|-------------------------|-------------------------------------------------|
| `assign_me`             | add the bot to the assignees                    |
| `unassign`              | remove **only** the bot from the assignees (a human's stays) |
| `label_add "<name>"`    | add a named label                               |
| `label_remove "<name>"` | remove a named label (by name)                  |
| `close`                 | close the issue (`state=closed`)                |
| `comment "<text>"`      | post a comment on the issue; `@{run:…}` interpolates run facts in `on_done`/`on_fail`, rejected in `on_claim` |

The actions of one block run in this order, not the order they are written in:
`assign_me`, then `label_remove`, `label_add`, `comment`, `unassign`, and `close` last. Two
of the same action run in the order written. Every block below reads the same either way —
see [why](#where-it-differs-from-the-built-in).

**Cadence.** Polls the forge every `poll_interval` for open issues carrying `source_label`
(if set); a service works **one issue at a time, to completion**, and `max_attempts` bounds
the per-issue retries. There is no clarification gate: a run that ends parked runs
`on_fail`, as one that fails does.

```conf
service widgets {
  work_dir "/srv/acme/widgets"
  trigger github {
    host         "github.com"
    repo         "acme/widgets"
    token        "REPLACE_ME"
    source_label "afkd/ready"

    on_claim { assign_me; label_add "afkd/working" }
    on_done { label_remove "afkd/working"; close }
    on_fail { label_remove "afkd/working"; unassign }
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
}
```

**Saying what happened.** A `comment` in `on_done`/`on_fail` can carry the run's own facts:
`@{run:duration}`, `@{run:cost}`, `@{run:turns}` and `@{run:name}` (the run's directory).

```conf
service widgets {
  work_dir "/srv/acme/widgets"
  trigger github {
    host  "ghe.example.com"
    repo  "acme/widgets"
    token "REPLACE_ME"

    on_done { comment "Fixed in @{run:duration} for @{run:cost}."; close }
    on_fail { comment "Gave up after @{run:turns} turns — log: @{run:name}/run.log"; unassign }
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
}
```

**Talking to a run in progress.** The brief is a snapshot taken at claim time, so a comment
posted while the agent is working is invisible to the run it is about — and on a repo whose
`on_done` closes the issue, it is invisible for good. `follow_comments <duration>` closes
that window. With it set, afkd asks the plugin for the claimed issue's comments on that
interval for the extent of the run and hands any new one to the agent **that is already
working**, as another turn in the same conversation:

```conf
service widgets {
  work_dir "/srv/acme/widgets"
  trigger github {
    repo            "acme/widgets"
    token           "REPLACE_ME"
    poll_interval   4m..6m
    follow_comments 60s          # the agent hears you mid-run
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
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

## Where it differs from the built-in

The plugin speaks afkd's plugin wire rather than living inside afkd, and the wire shapes a
few things. Each is deliberate, and none changes what a config means.

- **A block's actions run in a fixed order.** afkd hands a block to a plugin as a JSON
  object, which keeps the order of repeated actions but not the order *across* them. So a
  block runs `assign_me`, `label_remove`, `label_add`, `comment`, `unassign`, `close`, with
  repeats in written order. That is the order every block on this page is written in; a
  block written otherwise — `on_done { close; label_add "shipped" }` — adds the label
  before it closes, and if the add fails the issue is left open.
- **Some settings are refused when the service starts, not at `afkd validate`.** afkd holds
  the block to the plugin's declared keys before the plugin ever runs. What it cannot see —
  an empty `repo`, a `label_add` naming no label, a `comment` with no text, a `@{run:…}` in
  `on_claim` — the plugin refuses when it is greeted, in the built-in's own words, and the
  service ends there.
- **A lifecycle that did not land is held for afkd to release.** When `on_done`/`on_fail`
  cannot be carried out, the built-in keeps the claim and its reaper releases it on a later
  poll. The plugin answers afkd's `finish` with `held`, and afkd does the same: it keeps
  the claim and asks the plugin to release it — `afkd/claimed`, the bot's assignment and
  the marker — on a later beat, so the issue is retried from the top. This needs an afkd
  that understands `held`; an older one ignores it and treats the finish as delivered, and
  the issue keeps `afkd/claimed` and the bot's assignment until a human clears them.
- **The identity is looked up by whichever call needs it first.** A release unassigns the
  bot by its login, and afkd may ask for a release before it ever polls — after a restart,
  for a claim a crashed run left. If GitHub will not say who the token belongs to, the
  plugin writes nothing and afkd keeps the claim to ask again, as the built-in's reaper
  waits for the identity.
- **No stop mid-claim.** The built-in abandons a claim a shutdown lands in the middle of;
  the plugin cannot see afkd stop, so it finishes the claim, and afkd hands the unit
  straight back with a release. The issue ends where the built-in leaves it.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole issue through the skill. A thread with more
  new comments than fit in one reply delivers the newest, and names the ones left out.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, and each claim attempt waits a second for a rival's marker to show, so a scan that
  keeps losing races stops after 20 and leaves the rest of the issues for the next poll.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  issue acme/widgets#7` where the built-in's opens `# Work item from github issue
  acme/widgets#7`, and a comment delivered mid-run calls the issue "this work item" rather
  than "this issue".
