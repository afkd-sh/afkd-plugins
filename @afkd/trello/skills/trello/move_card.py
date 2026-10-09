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
# PLACEMENT: --above CARD / --below CARD put the card directly above or below a card
# already in the target list (CARD is its id or short link), as create_card.py
# does: one GET of the list's open cards and their `pos` once the list id is
# resolved, then the PUT with the number half-way between CARD and its neighbour,
# or `top`/`bottom` when CARD is at that end. The moved card itself is left out of
# the neighbours (matched by id or short link, whichever --card is), so a re-place
# inside its own list lands right, and a CARD that is the moved card is refused. A
# CARD not in the list, or a list read that fails, exits non-zero before the PUT,
# so nothing moves.
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


def placement(base, list_id, anchor, above, query, moving):
    """The `pos` that puts `moving` directly above (or below) `anchor` in the list.

    Both are matched against each open card's id and short link, and `moving` is
    left out of the neighbours. The answer is the mean of the anchor's `pos` and
    its neighbour's, or `top`/`bottom` when the anchor is at that end of the list.
    A failed read, an anchor not in the list, an anchor that is the moved card, or
    two neighbours with no float between them prints why and exits non-zero.
    """
    status, body = get(
        base, f"/1/lists/{list_id}/cards", f"fields=pos,shortLink&{query}"
    )
    if not 200 <= status < 300:
        print(
            f"move_card.py: failed to read cards for list {list_id}",
            file=sys.stderr,
        )
        sys.exit(1)

    def names(card, x):
        return x in (card.get("id"), card.get("shortLink"))

    cards = sorted(json.loads(body), key=lambda c: float(c.get("pos", 0)))
    target = next((c for c in cards if names(c, anchor)), None)
    if target is None:
        print(f"move_card.py: no card {anchor} in list {list_id}", file=sys.stderr)
        sys.exit(1)
    if names(target, moving):
        print(
            f"move_card.py: cannot place card {anchor} relative to itself",
            file=sys.stderr,
        )
        sys.exit(2)
    cards = [c for c in cards if not names(c, moving)]
    at = cards.index(target)
    if above and at == 0:
        return "top"
    if not above and at == len(cards) - 1:
        return "bottom"
    lo, hi = (cards[at - 1], cards[at]) if above else (cards[at], cards[at + 1])
    lo, hi = float(lo.get("pos", 0)), float(hi.get("pos", 0))
    pos = (lo + hi) / 2
    if not lo < pos < hi:
        print(
            f"move_card.py: no position left between {anchor} and its neighbour; "
            f"move one of them first",
            file=sys.stderr,
        )
        sys.exit(1)
    return pos


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not error on a missing positional; the hand-rolled
    # guard below enforces "exactly one of list_id / --name" with exit 2.
    parser.add_argument("list_id", nargs="?", help="the target list id")
    parser.add_argument("--name", help="a list's name to resolve to an id")
    # Where in the list the card lands: an end (by default the top, matching the
    # DSL `move_to`'s positionless default), or directly above or below a named
    # card. Only one may be given: argparse rejects a pair (exit 2) before any
    # network.
    place = parser.add_mutually_exclusive_group()
    place.add_argument("--pos", choices=["top", "bottom"], default="top")
    place.add_argument("--above", metavar="CARD", help="move it directly above CARD")
    place.add_argument("--below", metavar="CARD", help="move it directly below CARD")
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

    query = urllib.parse.urlencode(creds)

    # --- The position: --pos as given, else the place next to the named card in
    #     the resolved list, read before anything is written. ---
    pos, where = args.pos, args.pos
    above = args.above is not None
    if above or args.below is not None:
        anchor = args.above if above else args.below
        pos = placement(base, list_id, anchor, above, query, card)
        where = f"{'above' if above else 'below'} {anchor}"

    # --- Move the card. A non-2xx (e.g. an auth failure or an unknown list) is a
    #     non-zero exit the agent surfaces. The token is never echoed. ---
    status, _ = put(base, f"/1/cards/{card}", {"idList": list_id, "pos": pos}, query)
    if not 200 <= status < 300:
        print(
            f"move_card.py: failed to move card {card} to list {list_id}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"move_card.py: moved card {card} to list {list_id} ({where})")


if __name__ == "__main__":
    main()
