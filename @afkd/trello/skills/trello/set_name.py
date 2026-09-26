#!/usr/bin/env python3
# set_name.py — rename the active Trello card (ADR-0046 §A).
#
# The title half of the card-body pair in the bundled `trello` skill (the
# description half is set_description.py): it replaces the `name` of the card this
# run works on, so a grooming agent that has rewritten a card's body can also put
# its title into house style instead of leaving the two out of step. It is thin API
# mechanics — it sets exactly the title it is given, replacing the whole field. On a
# failure it exits non-zero so the agent surfaces it rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# PAYLOAD SHAPE: the new `name` rides in a JSON REQUEST BODY, and only the creds
# ride in the query string. The rule for this skill is one shape per endpoint:
# every writer against PUT /1/cards/{id} sends its fields as a body, which is what
# set_description.py already does (and create_card.py against POST /1/cards). A
# title is short enough not to need it on 414 grounds alone, but it is
# agent-authored free text of unbounded length and arbitrary charset (`&`, `?`,
# `—`, quotes), so it belongs off the request line for the same reason a `desc`
# does. The trigger's Rust client rides its fields in the query against this same
# endpoint (crates/trello/src/client.rs) — a different layer, deliberately not
# copied here, because two shapes on one endpoint inside one skill would buy
# nothing but a saved header.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# rename at a local stub listener, mirroring the seam in list_comments.py.

import argparse
import http.client
import json
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR NEW TITLE>"


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def set_name(base, card, name, query):
    """PUT the card's new `name`; return (status, body-bytes).

    The title travels as a JSON body (see the header comment: one shape per
    endpoint); `query` carries the creds and nothing else. The token is never
    echoed.
    """
    payload = json.dumps({"name": name})
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
    # The title is required and has no env default: argparse emits its own error
    # (exit 2) when it is missing, before any network.
    parser.add_argument("title", help="the card's new title")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- Refuse the SKILL.md placeholder: never rename a card to the fill-me-in
    #     example. ---
    if args.title == PLACEHOLDER:
        print(
            "set_name.py: refusing to set the placeholder title; substitute your real title",
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
            f"set_name.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "set_name.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Rename the card. A non-2xx (e.g. an auth failure, an unknown card, or an
    #     empty title Trello itself rejects) is a non-zero exit the agent surfaces.
    #     The token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, _ = set_name(base, card, args.title, query)
    if not 200 <= status < 300:
        print(
            f"set_name.py: failed to rename card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f'set_name.py: renamed card {card} to "{args.title}"')


if __name__ == "__main__":
    main()
