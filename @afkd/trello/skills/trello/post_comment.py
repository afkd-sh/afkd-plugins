#!/usr/bin/env python3
# post_comment.py — post a comment back to the active Trello card (ADR-0017 §7).
#
# The post half of the bundled `trello` skill (its read half is
# list_comments.py): it posts a single comment to the card this run works on, via
# the Trello REST API, so the agent can report a finished plan, an unresolved
# blocker, or a changed approach back onto the card. The agent calls it through
# SKILL.md with the message as the sole argument; on a post failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# It does NOT read its own path from the env (CLAUDE_PLUGIN_ROOT is not exported
# into Bash-tool subprocesses — ADR-0017 Revision §5); SKILL.md passes the path on
# the command line, so this helper owns only the API mechanics.
#
# Unlike the sh version it replaces, it uses only the Python standard library
# (no curl): the comment text is url-encoded through the `json`/`urllib` stack so
# quotes, newlines, and unicode survive.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# post at a local stub listener, mirroring the seam in list_comments.py.

import argparse
import http.client
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def post_comment(base, card, text, key, token):
    """POST one comment to the card's actions; return (status, body-bytes).

    Reproduces the sh's `curl --get --request POST`: text/key/token ride in the
    query string with an empty body, matching the endpoint the trigger's REST
    client uses (crates/trello/src/client.rs::post_comment). The token is never
    echoed.
    """
    query = urllib.parse.urlencode({"text": text, "key": key, "token": token})
    conn = connection(base)
    try:
        conn.request("POST", f"/1/cards/{card}/actions/comments?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not emit its own error on a missing message;
    # the hand-rolled guard below preserves the sh's exact wording and exit code.
    parser.add_argument("message", nargs="?")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- The message: the single argument. ---
    if not args.message:
        print(
            "post_comment.py: a comment message is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    text = args.message

    # --- Refuse the SKILL.md placeholder: never post the fill-me-in example. ---
    if text == PLACEHOLDER:
        print(
            "post_comment.py: refusing to post the placeholder text; substitute your real comment",
            file=sys.stderr,
        )
        sys.exit(2)

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
            f"post_comment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "post_comment.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Post the comment VERBATIM. It carries no afkd marker: the trigger keys
    #     a comment's identity on the posting member id (Comment.author against
    #     afkd's own), so a reply is recognized as afkd's own — kept out of the
    #     "new human feedback" delta — however it reached the board. A non-2xx
    #     (e.g. an auth failure or a refused post) is a non-zero exit the agent
    #     surfaces; the token is never echoed. ---
    status, _ = post_comment(base, card, text, key, token)
    if not 200 <= status < 300:
        print(
            f"post_comment.py: failed to post the comment to card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"post_comment.py: posted a comment to card {card}")


if __name__ == "__main__":
    main()
