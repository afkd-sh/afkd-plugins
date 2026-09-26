---
name: gitlab
description: Read and post notes on the GitLab issue or merge request this run works on, read and post attachments on it, list the project's open issues, and open a merge request for the branch it committed. Use it to list what reviewers have already said, to fetch a file a human pasted into the issue/MR (a screenshot or log lives as a `/uploads/…` reference in the body text, not as something you can open directly) so you can look at it, to reply once you have addressed feedback (the reply is what marks the round done), to attach an artifact back onto the issue/MR, and to open an MR linking back to the originating issue — not routine chatter.
allowed-tools: Bash(*/post_comment.py *), Bash(*/open_mr.py *), Bash(*/list_comments.py *), Bash(*/list_comments.py), Bash(*/list_issues.py), Bash(*/fetch_attachment.py), Bash(*/post_attachment.py *)
requires-executables: [python3, git]
---

## Read the active issue/MR's notes

Run the bundled reader yourself through the Bash tool, with no argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/list_comments.py"`

It prints each note's author, time, and body. An issue/MR with no notes prints a
single `no comments on …` line and succeeds. Pass `--issue N` or `--mr N` to read
a different issue/MR than the active one (see **Retargeting a different issue/MR**
below).

## Post a note to the active issue/MR

Run the bundled helper yourself through the Bash tool, **substituting your real
message** for the placeholder. Never run it with the placeholder text — the helper
rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/post_comment.py" "<REPLACE WITH YOUR COMMENT>"`

Pass `--issue N` or `--mr N` to post to a different issue/MR than the active one
(see **Retargeting a different issue/MR** below).

## Retargeting a different issue/MR

Both comment helpers accept two mutually-exclusive override flags: `--issue N`
targets issue N, `--mr N` targets merge request N. Each flag selects **both** the
number and the path kind, because GitLab keeps issues and merge requests on
separate paths — so from an active MR you can reach an issue, and vice versa. Give
at most one; supplying both is an error. With neither flag, the helper targets the
active issue/MR. Use `list_issues.py` (below) to find an issue number to target.

## Attachments

A file a human pasted into the issue/MR — a screenshot, a log, a diagram — lands
on GitLab as a markdown `/uploads/<hash>/<file>` reference in the body text, so
`list_comments.py` shows only the reference, not the file. To see it, fetch every
referenced upload and `Read` the downloaded file:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/fetch_attachment.py"`

It scans the active issue/MR's description and its notes for `/uploads/…`
references, downloads each into `$AFKD_SCRATCH_DIR/attachments/<hash>/<basename>`,
and prints the written path(s) — open one with the `Read` tool to view an image.
An issue/MR with no references prints a single `no attachments on …` line and
succeeds.

To hand an artifact back — an annotated screenshot, a rendered diff, a report —
attach a local file to the active issue/MR:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/post_attachment.py" "/path/to/file"`

It uploads the file to the project, then posts a note embedding the returned
markdown so the file renders on the issue/MR. GitLab's upload size limit is
server-configured; an over-limit file is rejected by the server and surfaces as a
non-zero exit.

## List the project's open issues

To find an issue number to cross-reference (e.g. to retarget a comment helper with
`--issue N`), list the project's open issues:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/list_issues.py"`

It prints one `#<iid>  <title>` line per open issue. A project with none prints a
single `no open issues on …` line and succeeds.

## Open a merge request

Once you have committed your changes on a branch, run the bundled helper, passing
the MR title as its single argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitlab/open_mr.py" "your MR title"`

It pushes the current branch to the origin remote (token-authenticated) and opens
a merge request against the target branch, embedding a `Closes #N` line in the
description when the originating issue number is known, so merging the MR
auto-closes it.

## Required environment

Every helper reads the issue/MR number and credentials from the environment afkd
already merged into this run; you do not pass them (the path is passed on the
command line — `CLAUDE_PLUGIN_ROOT` is not exported into Bash subprocesses,
ADR-0017). The helpers need `python3` on the host (and `git`, for `open_mr`):

- `GITLAB_TOKEN` / `GITLAB_BASE_URL` / `GITLAB_PROJECT` — credentials and the target
  project, required by every helper. `GITLAB_BASE_URL` is resolved to the REST API
  base (empty → `https://gitlab.com`; any other value → its `/api/v4` root, cloud or
  self-managed). `GITLAB_PROJECT` is a numeric id or a `group/name` path.
- The active issue/MR number — `GITLAB_MR_NUMBER`, then `GITLAB_ISSUE_NUMBER`,
  falling back to the number under `$AFKD_SCRATCH_DIR/mr/number` or
  `$AFKD_SCRATCH_DIR/issue/number` (the comment and attachment helpers use both;
  the MR helper's `Closes #N` linkage uses `GITLAB_ISSUE_NUMBER` then
  `$AFKD_SCRATCH_DIR/issue/number`). `list_comments.py`/`post_comment.py` accept
  `--issue N` / `--mr N` to override it for a different issue/MR (`list_issues.py`
  needs no number — it lists the whole project).
- `AFKD_SCRATCH_DIR` — additionally required by `fetch_attachment.py` (the
  download destination).
- `GITLAB_MR_TARGET` — the MR target branch, defaulting to `main`.

## Failure handling

If any helper exits non-zero it has failed — **surface that failure** (report it
as a blocker), do not retry it blindly.
