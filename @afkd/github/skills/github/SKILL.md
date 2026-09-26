---
name: github
description: Read and post comments on the GitHub issue or pull request this run works on, read image attachments a reviewer pasted into it, and open a pull request for the branch it committed. Use it to list what reviewers have already said, to fetch an image a human pasted into the issue/PR (a screenshot or diagram lives as an embedded image URL in the body/comment markdown, not as something you can open directly) so you can look at it, to reply once you have addressed feedback (the reply is what marks the round done), and to open a PR linking back to the originating issue — not routine chatter. GitHub has no attachment-upload API, so to hand an image back you link an externally hosted one in a comment.
allowed-tools: Bash(*/post_comment.py *), Bash(*/open_pr.py *), Bash(*/list_comments.py *), Bash(*/list_comments.py), Bash(*/fetch_attachment.py *)
requires-executables: [python3, git]
---

## Read the active issue/PR's comments

Run the bundled reader yourself through the Bash tool, with no argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/github/list_comments.py"`

It prints each comment's author, time, and body. An issue/PR with no comments
prints a single `no comments on …` line and succeeds. Pass `--issue N` to read a
different issue or pull request than the active one (GitHub keeps issue and PR
comments on the same path, so one flag reaches either).

## Post a comment to the active issue/PR

Run the bundled helper yourself through the Bash tool, **substituting your real
message** for the placeholder. Never run it with the placeholder text — the helper
rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/github/post_comment.py" "<REPLACE WITH YOUR COMMENT>"`

Pass `--issue N` to post to a different issue or pull request than the active one.

## Attachments (inbound only)

An image a human pasted into the issue/PR — a screenshot, a stack trace, a diagram
— lands on GitHub as an embedded image URL in the body/comment markdown, so
`list_comments.py` shows only the URL, not the picture. To see it, fetch every
embedded image and `Read` the downloaded file:

!`"${CLAUDE_PLUGIN_ROOT}/skills/github/fetch_attachment.py"`

It scans the active issue/PR's body and its comments for GitHub's embedded-image
URL shapes, downloads each into `$AFKD_SCRATCH_DIR/attachments/`, and prints the
written path(s) — open one with the `Read` tool to view an image. An issue/PR with
no embedded image prints a single `no attachments on …` line and succeeds.

GitHub's REST API has **no** way to upload or post an attachment onto an issue or
comment, so there is no upload helper. To hand an image back — an annotated
screenshot, a rendered diff — host it somewhere externally and **link** it in a
comment with `post_comment.py`; do not attempt an upload.

## Open a pull request

Once you have committed your changes on a branch, run the bundled helper, passing
the PR title as its single argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/github/open_pr.py" "your PR title"`

It pushes the current branch to the origin remote (token-authenticated) and opens
a pull request against the base branch, embedding a `Closes #N` line in the body
when the originating issue number is known, so merging the PR auto-closes it.

## Required environment

All three helpers read the issue/PR number and credentials from the environment
afkd already merged into this run; you do not pass them (the path is passed on the
command line — `CLAUDE_PLUGIN_ROOT` is not exported into Bash subprocesses,
ADR-0017). The helpers need `python3` on the host (and `git`, for `open_pr`):

- `GITHUB_TOKEN` / `GITHUB_HOST` / `GITHUB_REPO` — credentials and the target
  repo, required by every helper. `GITHUB_HOST` is resolved to the REST API base
  (empty or `github.com` → GitHub cloud; any other host → GitHub Enterprise
  Server).
- The active issue/PR number — `GITHUB_PR_NUMBER`, then `GITHUB_ISSUE_NUMBER`,
  falling back to the number under `$AFKD_SCRATCH_DIR/pr/number` or
  `$AFKD_SCRATCH_DIR/issue/number` (read/post/fetch use both; the PR helper's
  `Closes #N` linkage uses `GITHUB_ISSUE_NUMBER` then `$AFKD_SCRATCH_DIR/issue/number`).
  `list_comments.py`/`post_comment.py` accept `--issue N` to override it for a
  different issue/PR.
- `AFKD_SCRATCH_DIR` — additionally required by `fetch_attachment.py` (the
  download destination).
- `GITHUB_PR_BASE` — the PR base branch, defaulting to `main`.

## Failure handling

If any helper exits non-zero it has failed — **surface that failure** (report it
as a blocker), do not retry it blindly.
