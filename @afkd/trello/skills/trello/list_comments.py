#!/usr/bin/env python3
# list_comments.py — read the active Trello card's comments (ADR-0017 §7).
#
# The read half of the bundled `trello` skill (its post half is
# post_comment.py): it lists the comments on the card this run works on, via the
# Trello REST API, and prints each comment's author, time, and text. The agent
# calls it through SKILL.md with no argument (it reads everything from the
# environment); on a fetch or auth failure it exits non-zero so the agent
# surfaces the failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# It does NOT read its own path from the env (CLAUDE_PLUGIN_ROOT is not exported
# into Bash-tool subprocesses — ADR-0017 Revision §5); SKILL.md passes the path on
# the command line, so this helper owns only the API mechanics.
#
# Unlike the sh version it replaces, it parses the actions array with the `json`
# stdlib module (no jq): comment text is arbitrary (quotes, newlines, unicode),
# so a hand-parse would corrupt it. --limit bounds how many comments the API
# returns (default 50, Trello's own implicit default).
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# fetch at a local stub listener, mirroring the TrelloClient::with_base testing
# seam (crates/trello/src/client.rs).

import argparse
import http.client
import json
import os
import sys
import urllib.parse


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def get_comments(base, card, query):
    """GET the card's comment actions with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/cards/{card}/actions?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(action):
    """One block per comment: author + time, then the text.

    Author degrades best-available full name → username → the opaque member id,
    matching the sh/jq template (`fullName // username // idMemberCreator`), then
    the `date` and `data.text`; the two-space/newline layout mirrors it too.
    """
    creator = action.get("memberCreator") or {}
    author = (
        creator.get("fullName")
        or creator.get("username")
        or action.get("idMemberCreator")
    )
    date = action.get("date")
    text = (action.get("data") or {}).get("text", "")
    return f"{author}  {date}\n{text}\n"


def main():
    parser = argparse.ArgumentParser()
    # type=int makes a non-numeric --limit an argparse error (exit 2) before any
    # network. The default equals Trello's own implicit cap, so sending it yields
    # the same list the sh version got by sending nothing.
    parser.add_argument("--limit", type=int, default=50)
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- Credentials: required, from the inherited environment. Name what is
    #     missing so the config author knows which var to wire (ADR-0023). ---
    key = os.environ.get("TRELLO_API_KEY")
    token = os.environ.get("TRELLO_TOKEN")
    missing = []
    if not key:
        missing.append("TRELLO_API_KEY")
    if not token:
        missing.append("TRELLO_TOKEN")
    if missing:
        print(
            f"list_comments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "list_comments.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the card's comment actions. A non-2xx (e.g. an auth failure) is a
    #     non-zero exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode(
        {"filter": "commentCard", "limit": args.limit, "key": key, "token": token}
    )
    status, body = get_comments(base, card, query)
    if not 200 <= status < 300:
        print(
            f"list_comments.py: failed to read comments for card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of comment actions (unlike Telegram's
    #     `{"result": …}`); an empty card is success, not an error. ---
    actions = json.loads(body)
    if not actions:
        print(f"no comments on card {card}")
        sys.exit(0)

    print("\n".join(render(a) for a in actions))


if __name__ == "__main__":
    main()
