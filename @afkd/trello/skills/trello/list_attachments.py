#!/usr/bin/env python3
# list_attachments.py — list a Trello card's attachments (ADR-0046 §B).
#
# The read half of the bundled `trello` skill's attachment surface (fetch is
# fetch_attachment.py, post is post_attachment.py): it lists the attachments on the
# card this run works on, one `id  name  mimeType  bytes` line each, so the agent
# can see the screenshot a human pasted into a comment — Trello stores such a paste
# as a CARD ATTACHMENT, not comment text — and pick the id to fetch. The agent
# calls it through SKILL.md with no argument (the card defaults from the
# environment); on a fetch or auth failure it exits non-zero so the agent surfaces
# the failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# The account-wide token means --card can reach any card the token can (ADR-0046 §A).
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# fetch at a local stub listener, mirroring the seam in list_comments.py.

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


def get_attachments(base, card, query):
    """GET the card's attachments with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/cards/{card}/attachments?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
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
            f"list_attachments.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "list_attachments.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the card's attachments. A non-2xx (e.g. an auth failure) is a
    #     non-zero exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, body = get_attachments(base, card, query)
    if not 200 <= status < 300:
        print(
            f"list_attachments.py: failed to read attachments for card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of attachments; an empty card is
    #     success, not an error (many cards carry none). ---
    attachments = json.loads(body)
    if not attachments:
        print(f"no attachments on card {card}")
        sys.exit(0)

    for att in attachments:
        print(
            f"{att.get('id')}  {att.get('name')}  "
            f"{att.get('mimeType')}  {att.get('bytes')}"
        )


if __name__ == "__main__":
    main()
