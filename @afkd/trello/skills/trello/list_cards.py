#!/usr/bin/env python3
# list_cards.py — list the cards in a Trello list (ADR-0046 §A).
#
# The second step of board navigation in the bundled `trello` skill (its first is
# list_lists.py): given a list id, it prints every card in that list, one
# `id  name` line each, so the agent can find the card it needs. The list id is a
# REQUIRED positional argument — unlike the card/board locators there is no env
# default, because a list is not tied to this run the way the active card is; the
# agent gets the id from list_lists.py. On a fetch or auth failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the board credentials FROM THE ENVIRONMENT — the env the afkd trello
# trigger already merges into the worker child (crates/trello/src/trigger.rs:
# TRELLO_API_KEY / TRELLO_TOKEN). The account-wide token means the list id can
# reach any list the token can — an intended, accepted widening (ADR-0046 §A).
# --limit bounds how many cards the API returns (default 50, mirroring
# list_comments.py).
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


def get_cards(base, list_id, query):
    """GET the list's cards with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/lists/{list_id}/cards?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # The list id is required and has no env default: argparse emits its own error
    # (exit 2) when it is missing, before any network.
    parser.add_argument("list_id", help="the list id (from list_lists.py)")
    # type=int makes a non-numeric --limit an argparse error (exit 2) before any
    # network. The default mirrors list_comments.py.
    parser.add_argument("--limit", type=int, default=50)
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
            f"list_cards.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the list's cards. A non-2xx (e.g. an auth failure) is a non-zero
    #     exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"limit": args.limit, "key": key, "token": token})
    status, body = get_cards(base, args.list_id, query)
    if not 200 <= status < 300:
        print(
            f"list_cards.py: failed to read cards for list {args.list_id}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of cards; an empty list is success. ---
    cards = json.loads(body)
    if not cards:
        print(f"no cards in list {args.list_id}")
        sys.exit(0)

    for card in cards:
        print(f"{card.get('id')}  {card.get('name')}")


if __name__ == "__main__":
    main()
