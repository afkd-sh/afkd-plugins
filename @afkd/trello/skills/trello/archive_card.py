#!/usr/bin/env python3
# archive_card.py — archive the active Trello card, taking it off the board (ADR-0046 §A).
#
# The terminal board verb of the bundled `trello` skill: it closes the card this run
# works on (Trello's word for archive), so an agent that has groomed a card and found
# it resolves to nothing — a bug that did not reproduce, a duplicate, work already
# done — can take it off the board rather than leaving it in the column. That is work
# a service normally leaves to the trello trigger's `archive` lifecycle action
# (crates/trello/src/settings.rs: LifecycleAction::Archive), which a service with
# deliberately empty on_claim/on_done/on_fail blocks never fires: afkd::discuss grooms
# in place with no lifecycle blocks at all, so every board write it makes goes through
# the skill, and without this helper the one verb *this card is over* had no path. On a
# failure it exits non-zero so the agent surfaces the failure rather than retrying
# blindly.
#
# --unarchive reverses it (the check_item.py --uncheck precedent): archiving is the one
# skill verb that removes a card from every list view, and the --card override means it
# can remove the wrong one — so the undo ships with it, since the only other recovery
# path is a human opening the Trello UI.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# PAYLOAD SHAPE: the `closed` flag rides in a JSON REQUEST BODY, and only the creds
# ride in the query string. The rule for this skill is one shape per endpoint: every
# writer against PUT /1/cards/{id} sends its fields as a body, which is what
# set_name.py / set_description.py / move_card.py already do (and create_card.py against
# POST /1/cards). A bare `closed=true` is fixed-width and would fit the request line,
# but making this the one query-string writer against that endpoint would buy one saved
# header in exchange for two shapes on one endpoint. The trigger's Rust client DOES ride
# `closed` in the query against this same endpoint
# (crates/trello/src/client.rs::archive_card) — a different layer, deliberately not
# copied here.
#
# Idempotent by construction: `closed=true` on an already-archived card is a 2xx no-op,
# so this helper never GETs the card first to decide — the same contract add_label.py
# documents (adding a label the card already has is a no-op success). There is no
# placeholder guard either: there is no free-text argument to guard.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so field
# behavior is unchanged; the override exists ONLY so tests can point the PUT at a local
# stub listener, mirroring the seam in list_comments.py.

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


def set_closed(base, card, closed, query):
    """PUT the card's `closed` state; return (status, body-bytes).

    The flag travels as a JSON body (see the header comment: one shape per endpoint);
    `query` carries the creds and nothing else. `closed` is a Python bool, so
    json.dumps emits a lowercase `true`/`false`. The token is never echoed.
    """
    payload = json.dumps({"closed": closed})
    conn = connection(base)
    try:
        conn.request(
            "PUT",
            f"/1/cards/{card}?{query}",
            payload.encode("utf-8"),
            {"Content-Type": "application/json"},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--unarchive",
        action="store_true",
        help="restore the card to the board instead of archiving it",
    )
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
            f"archive_card.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "archive_card.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    closed = not args.unarchive
    verb = "unarchive" if args.unarchive else "archive"
    done = "unarchived" if args.unarchive else "archived"

    # --- Set the card's closed state. A non-2xx (e.g. an auth failure or an unknown
    #     card) is a non-zero exit the agent surfaces; the token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, _ = set_closed(base, card, closed, query)
    if not 200 <= status < 300:
        print(
            f"archive_card.py: failed to {verb} card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"archive_card.py: {done} card {card}")


if __name__ == "__main__":
    main()
