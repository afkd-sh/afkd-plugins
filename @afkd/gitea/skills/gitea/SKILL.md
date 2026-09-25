---
name: gitea
description: Read and post comments on the Gitea issue or pull request this run works on, read and post attachments on it, ask a clarifying question that parks the issue until a human replies, open a pull request for the branch it committed, and file a follow-up issue for work found but deliberately not done here. Use it to list what reviewers have already said, to fetch a file a human attached (that is where a pasted screenshot or log lives, not in the comment text) so you can look at it, to ask before implementing when the issue is genuinely ambiguous, to reply once you have addressed feedback (the reply is what marks the round done), to attach an artifact back onto the issue/PR, to open a PR linking back to the originating issue, and to file out-of-scope work as its own issue — either left for a human or queued for afkd to start on at once — not routine chatter.
allowed-tools: Bash(*/post_comment.py *), Bash(*/ask.py *), Bash(*/open_pr.py *), Bash(*/create_issue.py *), Bash(*/list_comments.py *), Bash(*/list_comments.py), Bash(*/list_attachments.py), Bash(*/fetch_attachment.py *), Bash(*/post_attachment.py *)
requires-executables: [python3, git]
---

## Read the active issue/PR's comments

Run the bundled reader yourself through the Bash tool, with no argument:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/list_comments.py"`

It prints each comment's author, time, and body. An issue/PR with no comments
prints a single `no comments on …` line and succeeds. Pass `--issue N` to read a
different issue/PR than the active one.

## Post a comment to the active issue/PR

Run the bundled helper yourself through the Bash tool, **substituting your real
message** for the placeholder. Never run it with the placeholder text — the helper
rejects it and exits non-zero.

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/post_comment.py" "<REPLACE WITH YOUR COMMENT>"`

Pass `--issue N` to post to a different issue/PR than the active one.

## Ask for clarification and park the run

When an issue does not give you enough to implement it confidently — and reading
the code and its comments has not resolved it — ask BEFORE doing the work. List
your specific questions, then run the helper with them as its single argument
(**substitute your real questions** for the placeholder):

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/ask.py" "<REPLACE WITH YOUR QUESTIONS>"`

In one step it posts your questions as a comment on the active issue **and** marks
the run "needs input". afkd then **parks** the issue: it drops the claim, labels
it `afkd/awaiting-reply`, and unassigns it — without closing it. When a human
replies, afkd re-claims it on a later poll and a fresh run starts with the answer
in the thread (read it with `list_comments.py`). After a successful call, **stop**
— do not start implementing; the work resumes in the next run. Ask only about
genuine ambiguities (unclear intent, a product decision, a missing acceptance
criterion), not things the code would answer.

## Attachments

A file a human attached to the issue/PR — a screenshot, a log — is stored by Gitea
as a structured **asset**, not as comment text, so `list_comments.py` will not show
it. To see it, list the assets on the issue/PR **and its comments**, fetch the one
you want, and `Read` the downloaded file:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/list_attachments.py"`

It prints one `id  name  size  browser_download_url` line per asset. An issue/PR
with none prints a single `no attachments on …` line and succeeds.

Then fetch a single asset by its id:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/fetch_attachment.py" "<ASSET-ID>"`

It downloads that asset into `$AFKD_SCRATCH_DIR/attachments/` and prints the
written path — open it with the `Read` tool to view an image.

To hand an artifact back — an annotated screenshot, a rendered diff, a report —
attach a local file to the active issue/PR:

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/post_attachment.py" "/path/to/file"`

Pass `--name "text"` to give the asset a display name (it defaults to the file's
basename). Gitea's upload size limit is server-configured; an over-limit file is
rejected by the server and surfaces as a non-zero exit.

## Open a pull request

Once you have committed your changes on a branch, run the bundled helper, passing
the PR title as its single argument. **Compose a real, specific title for the
change you made** — the `‹…›` below is a fill-in you MUST replace, not a value to
copy. A concise imperative summary of the diff works best (e.g.
`"Make admin users table its own scroll container"`):

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/open_pr.py" "‹one-line summary of your change›"`

It pushes the current branch to the origin remote (token-authenticated) and opens
a pull request against the base branch, embedding a `Closes #N` line in the body
when the originating issue number is known, so merging the PR auto-closes it. On
success it prints the new PR's number and URL (`opened pull request #N … : <url>`) —
use them to reference the PR (its `#N` or URL) in a follow-up comment on the issue.

It **refuses** two mistakes rather than opening a broken PR: a placeholder title
(`your PR title`, `title`, an un-replaced `‹…›`, …) exits non-zero asking for a real
one, and a branch with **no commits over the base** exits non-zero — open a PR only
after you have actually committed the change.

## File a follow-up issue

When you find work that is **outside the scope** of the issue or PR you were given —
a bug you noticed in passing, a refactor the change makes obvious, a test that should
exist — file it as its own issue and **stay on the work you were given**. Do not fix
it in the current PR, and do not bury it in a comment nobody re-reads. This is not for
chatter, and not for work you are about to do anyway as part of the current task.

By default the new issue is **inert**: it is opened, and nobody starts on it. That is
the right call unless the human has said the work should begin now.

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/create_issue.py" "‹one-line summary of the work›" --body "‹what is wrong and why it matters›"`

Two flags each hand the issue straight back to afkd, which claims it on its next poll
and **starts a run on it immediately**. Either one alone is enough; `--assign-me`
assigns the bot, and `afkd/ready` is the label afkd watches for.

!`"${CLAUDE_PLUGIN_ROOT}/skills/gitea/create_issue.py" "‹one-line summary of the work›" --body "‹what is wrong and why it matters›" --assign-me --label "afkd/ready"`

Pass them **only when the human has asked for the work to start immediately.** Adding
them speculatively enqueues autonomous work nobody asked for yet, and spends model
budget on it. When the human says to file it but not work on it now, pass neither.

Substitute a real title for the `‹…›` fill-in — a placeholder exits non-zero, as with
`open_pr.py`. Use `--body-file PATH` instead of `--body` for a long, multi-line body,
and repeat `--label` to add more than one. The issue is filed on the same repo as this
run, and its body gets a `Filed from #N.` line pointing back at the issue/PR you were
working on; pass `--from N` to name a different source, or `--no-footer` to write your
own backlink (or none). On success it prints the new issue's number and URL, and says
plainly whether anything will pick it up.

## Required environment

Every helper reads the issue/PR number and credentials from the environment afkd
already merged into this run; you do not pass them (the path is passed on the
command line — `CLAUDE_PLUGIN_ROOT` is not exported into Bash subprocesses,
ADR-0017):

- `GITEA_TOKEN` / `GITEA_BASE_URL` / `GITEA_REPO` — credentials and the target
  repo, required by every helper.
- The active issue/PR number — `GITEA_PR_NUMBER`, then `GITEA_ISSUE_NUMBER`,
  falling back to the number under `$AFKD_SCRATCH_DIR/pr/number` or
  `$AFKD_SCRATCH_DIR/issue/number` (the comment and attachment helpers use both;
  the PR helper's `Closes #N` linkage uses `GITEA_ISSUE_NUMBER` then
  `$AFKD_SCRATCH_DIR/issue/number`). `list_comments.py`/`post_comment.py` accept
  `--issue N` to override it for a different issue/PR.
- `AFKD_SCRATCH_DIR` — additionally required by `fetch_attachment.py` (the
  download destination) and by `ask.py` (where its park marker, `park`, is
  written). Both fail loudly when it is unset.
- `GITEA_PR_BASE` — the PR base branch, defaulting to `main`.

`create_issue.py` needs only the three credentials — it files onto `GITEA_REPO`, the
repo this run works on, and there is no way to point it at another. Its `--from`
default reads the active issue/PR the same way the comment helpers do. Every name
passed to `--label` must **already exist** in the repo: Gitea never creates a label,
so an undefined name fails the call (and the helper then leaves the issue unassigned
rather than half-enqueued). `afkd/ready` is the conventional name for the label afkd
picks issues up by, but it must match the `source_label` set in the service's
`trigger gitea { … }` block — if that block names a different label, use that one.

## Failure handling

If any helper exits non-zero it has failed — **surface that failure** (report it
as a blocker), do not retry it blindly.
