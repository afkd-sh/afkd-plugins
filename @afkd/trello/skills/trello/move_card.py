#!/usr/bin/env python3
# move_card.py — move the active Trello card to another list (ADR-0046 §A).
#
# The board-move verb of the bundled `trello` skill (its read half is
# list_lists.py): it puts the card this run works on into another list, so an agent
# grooming a card in place can take it out of the grooming column when it is done —
# work a service normally leaves to the trello trigger's `move_to` lifecycle
# action, which a service with deliberately empty on_claim/on_done/on_fail blocks
# never fires. The target list is named by its id (a positional arg) OR by --name
# (an exact list name), which this helper resolves against the card's own board. On
# a failure it exits non-zero so the agent surfaces the failure rather than
# retrying blindly.
#
# This is the agent-invoked (skill) counterpart to the trello TRIGGER's
# LifecycleAction::MoveTo (crates/trello/src/settings.rs), which afkd fires on
# claim/done/fail; that lifecycle action is untouched here. --pos follows it rather
# than create_card.py: a positionless `move_to` defaults to `top`, so this helper
# does too — filing NEW work at the end of a queue (create_card.py's `bottom`) is a
# different verb.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# The board a --name resolves against is read from the card (its idBoard), not from
# $TRELLO_BOARD_ID, so a by-name move never lands on another board's list.
#
# PAYLOAD SHAPE: idList/pos ride in a JSON REQUEST BODY, and only the creds ride in
# the query string. The rule for this skill is one shape per endpoint: every writer
# against PUT /1/cards/{id} sends its fields as a body, which is what
# set_description.py already does (and create_card.py against POST /1/cards, `pos`
# included). A list id is fixed-width and would fit on the request line, but making
# this the one query-string writer against that endpoint would buy one saved header
# in exchange for two shapes on one endpoint. The trigger's Rust client does ride
# idList/pos in the query against this same endpoint
# (crates/trello/src/client.rs::move_card) — a different layer, deliberately not
# copied here.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# calls at a local stub listener, mirroring the seam in list_comments.py.

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


def put(base, path, payload, query):
    """PUT `path?query` with a JSON `payload`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request(
            "PUT",
            f"{path}?{query}",
            json.dumps(payload).encode("utf-8"),
            {"Content-Type": "application/json"},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def card_board(base, card, creds):
    """Read the card's `idBoard` — the board a --name resolves against."""
    query = urllib.parse.urlencode({"fields": "idBoard", **creds})
    status, body = get(base, f"/1/cards/{card}", query)
    if not 200 <= status < 300:
        print(
            f"move_card.py: failed to read card {card}",
            file=sys.stderr,
        )
        sys.exit(1)
    return json.loads(body).get("idBoard")


def resolve_list_id(base, board, creds, name):
    """Resolve --name to a single list id on `board`.

    Returns the id on a unique match; prints a clear error and exits non-zero when
    the name matches no list or more than one (Trello permits duplicate list names,
    and there is no --color analogue to disambiguate with, so the id is the way
    out).
    """
    query = urllib.parse.urlencode(creds)
    status, body = get(base, f"/1/boards/{board}/lists", query)
    if not 200 <= status < 300:
        print(
            f"move_card.py: failed to read lists for board {board}",
            file=sys.stderr,
        )
        sys.exit(1)
    lists = json.loads(body)
    matches = [lst for lst in lists if lst.get("name") == name]
    if not matches:
        print(
            f"move_card.py: no list named {name!r} on board {board}",
            file=sys.stderr,
        )
        sys.exit(1)
    if len(matches) > 1:
        print(
            f"move_card.py: {name!r} is ambiguous — {len(matches)} lists match; "
            f"pass the list id (from list_lists.py)",
            file=sys.stderr,
        )
        sys.exit(1)
    return matches[0].get("id")


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not error on a missing positional; the hand-rolled
    # guard below enforces "exactly one of list_id / --name" with exit 2.
    parser.add_argument("list_id", nargs="?", help="the target list id")
    parser.add_argument("--name", help="a list's name to resolve to an id")
    # A moved card goes to the top of its new list, matching the DSL `move_to`'s
    # positionless default.
    parser.add_argument("--pos", choices=["top", "bottom"], default="top")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- Selection guard: exactly one of the positional id / --name. ---
    if bool(args.list_id) == bool(args.name):
        print(
            "move_card.py: pass exactly one of a list id or --name",
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
            f"move_card.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "move_card.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")
    creds = {"key": key, "token": token}

    # --- Resolve the target list id: the positional id verbatim (no card read at
    #     all), else --name against the card's own board (0 → unknown, >1 →
    #     ambiguous; both non-zero, no mutation). ---
    if args.list_id:
        list_id = args.list_id
    else:
        list_id = resolve_list_id(base, card_board(base, card, creds), creds, args.name)

    # --- Move the card. A non-2xx (e.g. an auth failure or an unknown list) is a
    #     non-zero exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode(creds)
    status, _ = put(
        base, f"/1/cards/{card}", {"idList": list_id, "pos": args.pos}, query
    )
    if not 200 <= status < 300:
        print(
            f"move_card.py: failed to move card {card} to list {list_id}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"move_card.py: moved card {card} to list {list_id} ({args.pos})")


if __name__ == "__main__":
    main()
