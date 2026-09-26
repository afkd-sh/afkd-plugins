#!/usr/bin/env python3
# list_labels.py — list the board's labels, flagging those on the active card
# (ADR-0046 §A).
#
# The read step of the label trio in the bundled `trello` skill (its mutating
# halves are add_label.py / remove_label.py): labels are defined at BOARD level and
# add/remove operate on label ids, so this helper resolves the board from the
# active card, then prints every board label as `id  color  name`, marking the ones
# already on the card with `[on card]`. That is how the agent discovers the id it
# will pass to add_label.py / remove_label.py. On a fetch or auth failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# The board is NOT read from $TRELLO_BOARD_ID: it is resolved from the card itself
# (the card read returns idBoard alongside the idLabels this helper needs anyway),
# so the flags stay correct even under a --card override onto another board.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# fetch at a local stub listener, mirroring the seam in list_checklists.py.

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


def get(base, path, query):
    """GET `path?query`; return (status, body-bytes). The token is never echoed."""
    conn = connection(base)
    try:
        conn.request("GET", f"{path}?{query}")
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
            f"list_labels.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "list_labels.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")
    creds = {"key": key, "token": token}

    # --- Resolve the board from the card, and read the card's current labels in
    #     the same call. A non-2xx (e.g. an auth failure) is a non-zero exit. ---
    query = urllib.parse.urlencode({"fields": "idBoard,idLabels", **creds})
    status, body = get(base, f"/1/cards/{card}", query)
    if not 200 <= status < 300:
        print(
            f"list_labels.py: failed to read card {card}",
            file=sys.stderr,
        )
        sys.exit(1)
    card_obj = json.loads(body)
    board = card_obj.get("idBoard")
    on_card = set(card_obj.get("idLabels") or [])

    # --- Read the board's defined labels. ---
    query = urllib.parse.urlencode(creds)
    status, body = get(base, f"/1/boards/{board}/labels", query)
    if not 200 <= status < 300:
        print(
            f"list_labels.py: failed to read labels for board {board}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of labels; a board with none is success,
    #     not an error (mirrors list_lists.py / list_checklists.py). ---
    labels = json.loads(body)
    if not labels:
        print(f"no labels defined on board {board}")
        sys.exit(0)

    for label in labels:
        marker = "  [on card]" if label.get("id") in on_card else ""
        print(
            f"{label.get('id')}  {label.get('color', '')}  "
            f"{label.get('name', '')}{marker}"
        )


if __name__ == "__main__":
    main()
