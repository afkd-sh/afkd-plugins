#!/usr/bin/env python3
# list_lists.py — list the lists (columns) on a Trello board (ADR-0046 §A).
#
# The board-navigation entry point of the bundled `trello` skill: it prints every
# list on a board, one `id  name` line each, so the agent can find the list id it
# needs to enumerate cards (list_cards.py) or move work around. The agent calls it
# through SKILL.md with no argument (the board defaults from the environment); on a
# fetch or auth failure it exits non-zero so the agent surfaces the failure rather
# than retrying blindly.
#
# It reads the board id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_BOARD_ID).
# The board is --board if given, else $TRELLO_BOARD_ID; there is no scratch
# fallback. The account-wide token means --board can reach any board the token can,
# not just this run's — an intended, accepted widening (ADR-0046 §A).
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


def get_lists(base, board, query):
    """GET the board's lists with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/boards/{board}/lists?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--board", help="board id (defaults to $TRELLO_BOARD_ID)")
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
            f"list_lists.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The board id: --board, else TRELLO_BOARD_ID. No scratch fallback. ---
    board = args.board or os.environ.get("TRELLO_BOARD_ID")
    if not board:
        print(
            "list_lists.py: no board id (set TRELLO_BOARD_ID or pass --board)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the board's lists. A non-2xx (e.g. an auth failure) is a non-zero
    #     exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, body = get_lists(base, board, query)
    if not 200 <= status < 300:
        print(
            f"list_lists.py: failed to read lists for board {board}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of lists; an empty board is success. ---
    lists = json.loads(body)
    if not lists:
        print(f"no lists on board {board}")
        sys.exit(0)

    for lst in lists:
        print(f"{lst.get('id')}  {lst.get('name')}")


if __name__ == "__main__":
    main()
