#!/usr/bin/env python3
# set_description.py — replace the active Trello card's description (ADR-0046 §A).
#
# The write half of the card-description pair in the bundled `trello` skill (its
# read half is read_description.py): it replaces the description of the card this
# run works on, so a grooming agent can maintain its interpretation/plan inside the
# card's `<!-- afkd -->` region. The splicing is the agent's job — read the current
# desc, edit only its marker region, write the whole desc back — so this helper is
# thin API mechanics: it sets exactly the text it is given. On a failure it exits
# non-zero so the agent surfaces it rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# PAYLOAD SHAPE: the new `desc` rides in a JSON REQUEST BODY, and only the creds
# ride in the query string — the same rule create_card.py documents: a card
# description is easily multi-kilobyte and Trello's edge answers HTTP 414 for a
# query string that long (a 7 KB `desc` in the query 414s; the same payload as a
# JSON body succeeds).
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# update at a local stub listener, mirroring the seam in list_comments.py.

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


def set_description(base, card, desc, query):
    """PUT the card's new `desc`; return (status, body-bytes).

    The description travels as a JSON body (see the header comment: a query-string
    `desc` 414s); `query` carries the creds and nothing else. The token is never
    echoed.
    """
    payload = json.dumps({"desc": desc})
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
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    # A description must come from exactly one source: argparse rejects the pair
    # (exit 2) before any network. --desc-file is the one to use for a real
    # multi-line body.
    desc_source = parser.add_mutually_exclusive_group(required=True)
    desc_source.add_argument("--desc", help="the new description text")
    desc_source.add_argument("--desc-file", help="read the new description from this file")
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
            f"set_description.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "set_description.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The new description: --desc, else --desc-file's contents. The read
    #     happens before any connection is opened, so an unreadable path — missing,
    #     a directory, or unpermitted alike — never reaches the network. ---
    desc = args.desc
    if args.desc_file:
        try:
            with open(args.desc_file, encoding="utf-8") as handle:
                desc = handle.read()
        except OSError as err:
            print(
                f"set_description.py: cannot read the description file {args.desc_file}: {err.strerror}",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Replace the description. A non-2xx (e.g. an auth failure or an unknown
    #     card) is a non-zero exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, _ = set_description(base, card, desc, query)
    if not 200 <= status < 300:
        print(
            f"set_description.py: failed to set the description of card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"set_description.py: set the description of card {card}")


if __name__ == "__main__":
    main()
