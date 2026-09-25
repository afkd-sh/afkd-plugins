# `@afkd/gitea`

A **provider** plugin: the `gitea` trigger kind, which turns open issues on a Gitea
repository — or every repository of an org — into afkd runs, and reflects progress back
through each issue's assignee, labels and state. It also ships the `gitea` skill an agent
uses to answer the issue: open the pull request, post comments and attachments, and ask a
question that parks the issue until a human replies.

It is afkd's built-in Gitea issue trigger, moved out of afkd: the same keys, the same claim
markers and lifecycle comments on the issue, the same claim-journal keys and session
threads, the same run environment and the same brief. A claim the built-in left on a live
issue is recognised, renewed and released by the plugin, and the other way round, so
switching from one to the other strands nothing. The few places the plugin behaves
differently are listed [at the end](#where-it-differs-from-the-built-in). See
[`docs/plugins.md`](https://afkd.sh/docs/plugins/) for the wire it speaks.

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
the crates `Cargo.lock` pins (`ureq`, `serde`, `serde_json` and theirs). An afkd that
still has `gitea` compiled in refuses the install, because a plugin may not shadow a
built-in kind; use the built-in there, with exactly the same config.

## Name the skill

The skill is the plugin's own, so it is named with the plugin's name in front:

```conf
# The agent a `gitea` service runs: the skill is what lets it answer the issue.
agent fixer { worker claude { model sonnet; skills @afkd/gitea/gitea } }
```

Nothing hands it to an agent on its own; a `skills` list has to name it. It reads the
`GITEA_TOKEN`, `GITEA_BASE_URL`, `GITEA_REPO` and `GITEA_ISSUE_NUMBER` every run of this
kind carries.

## `gitea` (issues)

Fires for open issues on a Gitea repository — or every repository of an org — and
reflects progress back through the issue's assignee, labels, and state.

An issue is **up for grabs** when it is **assigned to the bot** — the user the `token`
authenticates as (e.g. a dedicated `autocoder` account) — *or* when it carries the optional
`source_label`. Assignment is the seamless path: point the bot's user at an issue and the
next poll picks it up, no label to remember. The `source_label` remains as a classic label
gate for those who want it; either signal suffices.

| Key             | Value             | Notes                                              |
|-----------------|-------------------|----------------------------------------------------|
| `base_url`      | `"<url>"`         | the Gitea instance base URL                        |
| `repo`          | `"owner/name"`    | a single repository; **exactly one** of `repo`/`org` |
| `org`           | `"<org>"`         | every repository of an org; **exactly one** of `repo`/`org` |
| `token`         | `"<token>"`       | personal access token; required                    |
| `source_label`  | `"<label>"`       | optional label gate; alternative to assigning bot  |
| `discuss_with`  | `anyone \| <login>…` | claim an issue on first sight (afkd never commented) or when an allowed author comments after afkd last did; default unset |
| `follow_comments` | `<duration>`    | re-read the issue this often **while its run is in flight**, delivering new comments to the working agent; a range jitters (see [Durations](https://afkd.sh/docs/configuration/#durations)); default unset (no mid-run watch) |
| `max_attempts`  | `<n>`             | retries per issue; default `1`                      |
| `poll_interval` | `<duration>`      | default `30s`; a range jitters (see [Durations](https://afkd.sh/docs/configuration/#durations)) |
| `on_claim`      | `{ <actions> }`   | issue-lifecycle actions when work starts           |
| `on_done`       | `{ <actions> }`   | issue-lifecycle actions when work finishes         |
| `on_fail`       | `{ <actions> }`   | issue-lifecycle actions when work fails            |
| `on_park`       | `{ <actions> }`   | optional extras when a run parks for a reply       |

**Exactly one** of `repo` (a single `owner/name`) or `org` (every repo of an org) is
required — naming neither or both is rejected. The lifecycle actions only manage the
issue's *status* — its assignee, labels, and state. The **claim** itself is a
`[afkd-claim]` marker comment the plugin posts and releases on its own; nothing here holds
or releases it. afkd keeps that marker **alive while the run is**, asking the plugin to
edit it every few minutes, so a run past an hour still holds its issue and a second
instance loses the race rather than double-claiming it; only a marker nobody is renewing
any more ages out. Opening the PR and posting the reply comment are the **agent's** job,
done through the `@afkd/gitea/gitea` skill, not through lifecycle verbs.

**The claim label.** The plugin holds its re-pick gate in `afkd/claimed`: it adds the label
itself when a claim is won, and an issue carrying it is never picked up again (a human
removes it to retry). It **creates that label** — and `afkd/awaiting-reply` — in the
repository when they are missing, as plain non-exclusive labels. Do not redefine either as
an **exclusive** scoped label: Gitea strips an exclusive label the moment another label in
the same scope is added, so the gate would vanish under your own `on_claim` and the issue
would be claimed again on every poll. A plugin that finds `afkd/claimed` already defined
exclusive refuses to run and says so. Relatedly, a `label_add` naming a label the
repository does not define is an **error** (Gitea silently ignores such names rather than
failing), and it aborts the rest of that lifecycle block.

The actions inside an `on_claim` / `on_done` / `on_fail` / `on_park` block act through
Gitea's native primitives:

| Action                  | Effect                                  |
|-------------------------|-----------------------------------------|
| `assign_me`             | assign the issue to the bot             |
| `unassign`              | remove the bot from the assignees (a human's stays) |
| `label_add "<name>"`    | add a named label                       |
| `label_remove "<name>"` | remove a named label                    |
| `close`                 | close the issue                         |
| `comment "<text>"`      | post a comment on the issue; `@{run:…}` interpolates run facts in `on_done`/`on_fail`/`on_park`, rejected in `on_claim` |

The actions of one block run in this table's order, not the order they are written in:
`assign_me`, then `label_remove`, `label_add`, `comment`, `unassign`, and `close` last. Two
of the same action run in the order written. Every block below reads the same either way —
see [why](#where-it-differs-from-the-built-in).

**Mind the spelling.** The label actions are written verb-last, `label_add` and
`label_remove`; `trello` writes the same two verb-first, `add_label` and `remove_label`.
Neither spelling is accepted by the other kind.

**Cadence.** Polls the forge every `poll_interval` for open issues **assigned to the bot**
(or carrying `source_label`, if set) across a single `repo` or every repo of an `org`; a
service works **one issue at a time, to completion**, and `max_attempts` bounds the
per-issue retries. The `on_*` actions are applied to the issue as work begins, finishes,
or fails.

```conf
service widgets {
  work_dir "/srv/acme/widgets"
  trigger gitea {
    base_url "https://gitea.example.com"
    repo     "acme/widgets"
    token    "REPLACE_ME"

    on_claim { assign_me; label_add "afkd/working" }
    on_done { label_remove "afkd/working"; close }
    on_fail { label_remove "afkd/working"; unassign }
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
}
```

**The clarification gate.** A run has a third outcome between finish and fail: when the
issue is too ambiguous to implement, the agent can **ask first**. Give a worker the
`@afkd/gitea/gitea` skill; its **ask** action posts the agent's questions on the issue and
writes the marker `park` into the run's scratch dir (`$AFKD_SCRATCH_DIR`). The plugin reads
that marker at the end of the run and *parks* the issue instead of failing it — drops the
claim, labels it `afkd/awaiting-reply`, removes the bot from the assignees, **without
closing it**. The label is managed by the plugin itself, so the gate holds even with no
`on_park` block; `on_park` is only for extras (a custom label, say). A parked issue is
**re-claimed automatically on a later poll once a human replies** with a comment newer than
the bot's last word — a fresh run starts with the answer in its brief. The exchange repeats
until the agent has what it needs, then it proceeds to `on_done` as usual.

The marker lives in scratch, not the working dir, so the gate works the same in-repo and
under `in_worktree` (scratch is outside the copy the engine tears down), and a fresh scratch
dir per attempt means no marker can outlive the run that wrote it. One requirement remains:

- **Stop the run at the ask, yourself.** The marker parks the issue, but the engine does
  not halt the workflow for you — so in a multi-step workflow, gate the rest on the marker
  so nothing runs after the ask:

  ```conf
  # The agent the service below calls. Minimal, so this snippet validates on its own.
  agent fixer { worker claude { model sonnet; skills @afkd/gitea/gitea } }

  service widgets {
    work_dir "/srv/acme/widgets"
    trigger gitea {
      base_url "https://gitea.example.com"
      repo     "acme/widgets"
      token    "REPLACE_ME"
      on_park  { comment "parked after @{run:duration} — waiting on you" }
    }
    run {
      run_agent fixer { prompt """… run the gitea skill's ask action if unclear …""" }
      if run_cmd "test -f $AFKD_SCRATCH_DIR/park" { fail "parked: awaiting a human reply" }
      # reviewer / commit / PR steps below never run when the agent asked
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
`discuss_with anyone` allows any author but the bot itself; `discuss_with alice bob` allows
only those Gitea logins (the bot's own login is struck from the list however it is written,
so it can never answer itself); on first sight the allow-list does not apply. The tail
boundary is decided by comment **author**, not marker text, so any comment posted as the
bot bounds it, and the whole tail is scanned — a drive-by comment from a disallowed author
cannot mask a still-unanswered allowed one. The retained replies are what the regenerated
brief hands the agent, filtered by the same allow-list.

Every gated turn ends with afkd as the last speaker: if the agent posts nothing, the plugin
posts one terse backstop comment (`reviewed, nothing to add`, `awaiting a human reply`, or
`run did not complete: …`) so the issue does not re-fire on the next poll. A `comment` in
`on_done`/`on_fail`/`on_park` counts as afkd speaking and suppresses it. Omitting the key
leaves the claim path unchanged — no comments are read at all outside the
`afkd/awaiting-reply` re-arm above; `anyone` is an explicit value, not the same as omitting
it.

```conf
service groom {
  work_dir "/srv/acme/widgets"
  trigger gitea {
    base_url     "https://gitea.example.com"
    repo         "acme/widgets"
    token        "REPLACE_ME"
    discuss_with alice bob
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
}
```

**Talking to a run in progress.** Everything above happens *between* runs: the brief is a
snapshot taken at claim time, so a comment posted while the agent is working is invisible
to the run it is about — and on a repo whose `on_done` closes the issue, it is invisible for
good. `follow_comments <duration>` closes that window. With it set, afkd asks the plugin
for the claimed issue's comments on that interval for the extent of the run and hands any
new one to the agent **that is already working**, as another turn in the same conversation:

```conf
service widgets {
  work_dir "/srv/acme/widgets"
  trigger gitea {
    base_url        "https://git.example.com"
    repo            "acme/widgets"
    token           "REPLACE_ME"
    poll_interval   4m..6m
    follow_comments 60s          # the agent hears you mid-run
  }
  run { run_cmd "cat $AFKD_SCRATCH_DIR/task.md" }
}
```

Every delivered message is **also appended to the run's `task.md`**, so the next agent of a
multi-agent run reads the correction in the task itself, and a retry keeps it. The agent
picks a delivered message up at its next tool boundary. Comments are delivered once each,
oldest-first, attributed exactly as a brief's `## New comments` section is; afkd's own
comments and `[afkd-claim]` markers are never delivered back. A comment that lands after
the last tick is picked up by the **run-end sweep** afkd makes before the `on_done`/`on_fail`
moment, and earns the issue another round of the same fire. Cost: one extra comment read
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
  both or neither of `repo`/`org`, a `label_add` naming no label, a `comment` with no text,
  a valueless `discuss_with`, a `@{run:…}` in `on_claim` — the plugin refuses when it is
  greeted, in the built-in's own words, and the service ends there.
- **A lifecycle that did not land is released at once.** When `on_done`/`on_fail`/the park
  cannot be carried out, the built-in holds the claim and releases it on its next poll.
  The plugin releases it straight away — drops `afkd/claimed` and the marker — so the next
  poll retries the issue. The second time the same issue fails that way it is left
  claimed, for a human, exactly as the built-in does; both say so in the service log.
- **One reply is at most 64 KiB.** A brief longer than that is cut to fit, with a note at
  the end telling the agent to read the whole issue through the skill. A thread with more
  new comments than fit in one reply delivers the newest, and names the ones left out.
- **One poll scans for at most 20 seconds.** afkd gives a plugin 60 seconds to answer a
  poll, so an org-wide scan against a slow forge stops after 20 and leaves the rest of the
  repositories for the next poll.
- **afkd frames the brief as a plugin's.** A run's `task.md` opens `# Work item from plugin
  issue acme/widgets#7` where the built-in's opens `# Work item from gitea issue
  acme/widgets#7`, and a comment delivered mid-run calls the issue "this work item" rather
  than "this issue".
