#!/usr/bin/env python3
# create_issue.py — file a follow-up issue on the active Gitea repo (ADR-0031).
#
# The "file a follow-up" action of the bundled `gitea` skill, the analogue of
# trello's create_card.py: given a title, it opens one issue on the repo this run
# works on, so an agent that finds work outside the current issue/PR's scope files
# it as a real issue and stays on the one it was given, instead of derailing onto
# the new work or burying it in a comment nobody re-reads. On a failure it exits
# non-zero so the agent surfaces it rather than retrying blindly.
#
# TWO INDEPENDENT ENQUEUE SIGNALS. The afkd gitea issue trigger treats an issue as
# a candidate when it is assigned to the bot OR carries the configured source label
# (crates/gitea/src/trigger_issue.rs: `candidates`) — either alone is enough. So
# --assign-me and --label "afkd/ready" each hand the new issue straight back to
# afkd, which claims it on its next poll and starts a run on it. With NEITHER (the
# default) the issue is created inert: open, unassigned, unlabeled, waiting for a
# human. SKILL.md carries that guardrail: the flags are for when the human asked
# for the work to start now, not for every follow-up the agent notices.
#
# It reads the credentials and the target repo FROM THE ENVIRONMENT — the env the
# afkd gitea triggers already merge into the worker child (crates/gitea/src/
# common.rs: GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO). There is deliberately no
# --repo override: unlike trello's account-wide token, which reaches any list and
# makes create_card.py's list id a real widening, keeping the target env-bound
# means a run can only file work into the repo it was already working in.
#
# It does NOT read its own path from the env (CLAUDE_PLUGIN_ROOT is not exported
# into Bash-tool subprocesses — ADR-0017 Revision §5); SKILL.md passes the path on
# the command line, so this helper owns only the API mechanics.

import argparse
import http.client
import json
import os
import sys
import urllib.parse

PLACEHOLDERS = {
    "your issue title",
    "issue title",
    "title",
    "your title",
    "<title>",
}


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def request(base, method, path, token, payload=None):
    """One authenticated API call; return (status, body-bytes). The token rides the
    Authorization header, never a query param, so it is not surfaced."""
    conn = connection(base)
    try:
        headers = {"Authorization": f"token {token}"}
        body = None
        if payload is not None:
            body = json.dumps(payload)
            headers["Content-Type"] = "application/json"
        conn.request(method, path, body=body, headers=headers)
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def ok(status):
    """Whether an HTTP status is a success."""
    return 200 <= status < 300


def source_number():
    """The issue/PR this run works on, for the provenance footer: GITEA_PR_NUMBER,
    then GITEA_ISSUE_NUMBER, then the <scratch>/pr/number → <scratch>/issue/number
    file fallbacks, else None. Same resolution order as post_comment.py."""
    number = os.environ.get("GITEA_PR_NUMBER") or os.environ.get("GITEA_ISSUE_NUMBER")
    if number:
        return number
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        for rel in ("pr", "issue"):
            path = os.path.join(scratch, rel, "number")
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as handle:
                    return handle.read().strip()
    return None


def is_placeholder(title):
    """Whether `title` is a fill-in rather than a real title. The SKILL.md example is
    a fill-in, and copying it verbatim (or passing a generic stand-in / an
    un-replaced `‹…›` guillemet form) yields a uselessly-titled issue."""
    normalized = title.strip().casefold()
    return normalized in PLACEHOLDERS or (
        normalized.startswith("‹") and normalized.endswith("›")
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("title", nargs="?", help="the new issue's title")
    # An issue may have an empty body, so both sources are optional — but a body
    # cannot come from two places at once: argparse rejects the pair (exit 2) before
    # any network. --body-file is the one to use for a real multi-line body.
    body_source = parser.add_mutually_exclusive_group()
    body_source.add_argument("--body", help="the issue's body text")
    body_source.add_argument("--body-file", help="read the body from this file")
    # The two enqueue signals. Either one alone makes afkd claim the issue on its
    # next poll; both are off by default, which files the issue inert.
    parser.add_argument(
        "--assign-me",
        action="store_true",
        help="assign the issue to the bot (afkd will start work on it)",
    )
    parser.add_argument(
        "--label",
        action="append",
        default=[],
        metavar="NAME",
        help="add a label, repeatable (afkd/ready makes afkd start work on it)",
    )
    # The provenance footer. It defaults to the run's own issue/PR, but a follow-up
    # can be filed from a context the env does not describe, so the agent may name
    # the source itself or write its own backlink into the body and suppress ours.
    footer = parser.add_mutually_exclusive_group()
    footer.add_argument(
        "--from",
        dest="filed_from",
        metavar="N",
        help="the issue/PR the follow-up came from (defaults to this run's)",
    )
    footer.add_argument(
        "--no-footer",
        action="store_true",
        help="do not append the `Filed from #N.` provenance line",
    )
    args = parser.parse_args()

    # --- The title: the sole positional. nargs="?" so the wording and exit code are
    #     ours, not argparse's. ---
    if not args.title:
        print(
            "create_issue.py: an issue title is required (pass it as the first argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    if is_placeholder(args.title):
        print(
            f"create_issue.py: {args.title!r} is a placeholder, not a real issue title — "
            "pass a concise summary of the work you are filing",
            file=sys.stderr,
        )
        sys.exit(2)

    # --- Credentials + repo: required, from the inherited environment. Name what is
    #     missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITEA_TOKEN")
    base = os.environ.get("GITEA_BASE_URL")
    repo = os.environ.get("GITEA_REPO")
    missing = [
        name
        for name, value in (
            ("GITEA_TOKEN", token),
            ("GITEA_BASE_URL", base),
            ("GITEA_REPO", repo),
        )
        if not value
    ]
    if missing:
        print(
            f"create_issue.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The body: --body, else --body-file's contents, else empty. The read happens
    #     before any connection is opened, so an unreadable path — missing, a
    #     directory, or unpermitted alike — never reaches the network. ---
    body = args.body or ""
    if args.body_file:
        try:
            with open(args.body_file, encoding="utf-8") as handle:
                body = handle.read()
        except OSError as err:
            print(
                f"create_issue.py: cannot read the body file {args.body_file}: {err.strerror}",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- The provenance footer: Gitea numbers issues and PRs in one sequence, so
    #     `#N` cross-links either. No source found ⇒ no footer: a follow-up filed
    #     outside a unit run is not a bug. ---
    if not args.no_footer:
        origin = args.filed_from or source_number()
        if origin:
            origin = origin.lstrip("#")
            body = f"{body.rstrip()}\n\nFiled from #{origin}." if body else f"Filed from #{origin}."

    # --- The bot's own login, when it is to be assigned. Resolved before the issue
    #     exists, so a bad token fails without leaving one behind. ---
    me = None
    if args.assign_me:
        status, raw = request(base, "GET", "/api/v1/user", token)
        if not ok(status):
            print(
                "create_issue.py: cannot resolve the authenticated user to assign the issue to",
                file=sys.stderr,
            )
            sys.exit(1)
        try:
            me = json.loads(raw).get("login")
        except (ValueError, AttributeError):
            me = None
        if not me:
            print(
                "create_issue.py: the authenticated user has no login to assign the issue to",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- Create the issue INERT: no assignees, no labels. The order of the three
    #     calls is the safety property. Assignment lands LAST because assignment
    #     alone enqueues the issue for afkd: if the labels fail, we exit before
    #     assigning, leaving an issue that is open but inert — a partial state a
    #     human triages, never one afkd picks up carrying the wrong labels. ---
    status, raw = request(
        base, "POST", f"/api/v1/repos/{repo}/issues", token, {"title": args.title, "body": body}
    )
    if not ok(status):
        print(
            f'create_issue.py: failed to create the issue "{args.title}" in {repo}',
            file=sys.stderr,
        )
        sys.exit(1)

    # The created issue's number and web URL. Defensive: a body that does not parse
    # still succeeds — the issue exists — but without a number there is nothing to
    # label or assign, so the enqueue flags cannot be honoured and that is a failure.
    number, html_url = None, None
    try:
        created = json.loads(raw)
        number = created.get("number")
        html_url = created.get("html_url")
    except (ValueError, AttributeError):
        pass
    if number is None:
        if args.label or args.assign_me:
            print(
                f"create_issue.py: created the issue in {repo} but its number could not be read, "
                "so it was neither labeled nor assigned; it will NOT be picked up",
                file=sys.stderr,
            )
            sys.exit(1)
        print(f'create_issue.py: created issue "{args.title}" in {repo}')
        return

    # --- Labels, by NAME. This endpoint resolves label names server-side — the same
    #     request crates/gitea/src/client.rs::add_label sends — so no name→id lookup
    #     is needed. Gitea never auto-creates a label: an undefined name is a non-2xx. ---
    if args.label:
        status, _ = request(
            base,
            "POST",
            f"/api/v1/repos/{repo}/issues/{number}/labels",
            token,
            {"labels": args.label},
        )
        if not ok(status):
            names = ", ".join(args.label)
            print(
                f"create_issue.py: created issue #{number} in {repo} but failed to apply its "
                f"labels ({names}) — each must already exist in the repo; the issue was left "
                "unassigned and will NOT be picked up",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- Assignment: last, and only now that the labels landed. ---
    if me:
        status, _ = request(
            base,
            "PATCH",
            f"/api/v1/repos/{repo}/issues/{number}",
            token,
            {"assignees": [me]},
        )
        if not ok(status):
            print(
                f"create_issue.py: created issue #{number} in {repo} but failed to assign it to "
                f"{me}; it will NOT be picked up",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- Report which of the two outcomes actually happened, so the run log says
    #     whether anything will pick the issue up. ---
    if me or args.label:
        did = []
        if me:
            did.append(f"assigned to {me}")
        if args.label:
            did.append(f"labeled {', '.join(args.label)}")
        fate = f" ({' and '.join(did)} — afkd will pick it up)"
    else:
        fate = " (unassigned, unlabeled — nobody will work on it yet)"
    url = f": {html_url}" if html_url else ""
    print(f"create_issue.py: created issue #{number} in {repo}{url}{fate}")


if __name__ == "__main__":
    main()
