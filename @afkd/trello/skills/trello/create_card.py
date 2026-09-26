#!/usr/bin/env python3
# create_card.py — file a new card onto a Trello list (ADR-0046 §A).
#
# The "file a follow-up" half of the bundled `trello` skill (its read half is
# list_cards.py): given a list id and a title, it creates one card in that list,
# so an agent that discovers follow-up work while running a card can file it as a
# real card and stay on the one it was given, instead of derailing onto the new
# work or burying it in a comment nobody re-reads. On a create failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# The list id is a REQUIRED positional argument — unlike the card/board locators
# there is no env default, because a list is not tied to this run the way the
# active card is; the agent gets the id from list_lists.py. The account-wide token
# means the list id can reach any list the token can — an intended, accepted
# widening (ADR-0046 §A), and a sharper edge here than for the readers: a card
# created into the trigger's `pick_from` list enqueues autonomous work for afkd
# itself. SKILL.md carries that guardrail; this helper never learns `pick_from`.
#
# It reads the board credentials FROM THE ENVIRONMENT — the env the afkd trello
# trigger already merges into the worker child (crates/trello/src/trigger.rs:
# TRELLO_API_KEY / TRELLO_TOKEN). No card id is read: the new card has none yet.
#
# PAYLOAD SHAPE: idList/name/desc/pos ride in a JSON REQUEST BODY, and only the
# creds ride in the query string. This is the one place create_card.py must not
# copy post_comment.py, which rides its text in the query: a card description is
# easily multi-kilobyte and Trello's edge answers HTTP 414 for a query string that
# long (verified empirically — a 7 KB `desc` in the query 414s; the same payload
# as a JSON body succeeds). post_attachment.py is the precedent for the resulting
# shape: a POST body with the creds in the query.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# create at a local stub listener, mirroring the seam in list_comments.py.

import argparse
import http.client
import json
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR CARD TITLE>"


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def create_card(base, list_id, name, desc, pos, query):
    """POST one card into the list; return (status, body-bytes).

    The card's fields travel as a JSON body (see the header comment: a query-string
    `desc` 414s); `query` carries the creds and nothing else. The token is never
    echoed.
    """
    payload = json.dumps({"idList": list_id, "name": name, "desc": desc, "pos": pos})
    conn = connection(base)
    try:
        conn.request(
            "POST",
            f"/1/cards?{query}",
            payload.encode("utf-8"),
            {"Content-Type": "application/json"},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # Both positionals are required and have no env default: argparse emits its own
    # error (exit 2) when either is missing, before any network.
    parser.add_argument("list_id", help="the list id (from list_lists.py)")
    parser.add_argument("title", help="the new card's title")
    # A card may have an empty description, so both sources are optional — but a
    # description cannot come from two places at once: argparse rejects the pair
    # (exit 2) before any network. --desc-file is the one to use for a real
    # multi-line body.
    desc_source = parser.add_mutually_exclusive_group()
    desc_source.add_argument("--desc", help="the card's description text")
    desc_source.add_argument("--desc-file", help="read the description from this file")
    # A filed follow-up joins the end of a list; it does not jump the queue.
    parser.add_argument("--pos", choices=["top", "bottom"], default="bottom")
    args = parser.parse_args()

    # --- Refuse the SKILL.md placeholder: never file the fill-me-in example. ---
    if args.title == PLACEHOLDER:
        print(
            "create_card.py: refusing to file the placeholder title; substitute your real title",
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
            f"create_card.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The description: --desc, else --desc-file's contents, else empty. The
    #     read happens before any connection is opened, so an unreadable path —
    #     missing, a directory, or unpermitted alike — never reaches the network. ---
    desc = args.desc or ""
    if args.desc_file:
        try:
            with open(args.desc_file, encoding="utf-8") as handle:
                desc = handle.read()
        except OSError as err:
            print(
                f"create_card.py: cannot read the description file {args.desc_file}: {err.strerror}",
                file=sys.stderr,
            )
            sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Create the card. A non-2xx (e.g. an auth failure or an unknown list) is a
    #     non-zero exit the agent surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, body = create_card(base, args.list_id, args.title, desc, args.pos, query)
    if not 200 <= status < 300:
        print(
            f'create_card.py: failed to create card "{args.title}" in list {args.list_id}',
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Report the short url, not the 24-hex id: it is the clickable one, so a
    #     human reading the log can open the card. ---
    created = json.loads(body)
    url = created.get("shortUrl") or created.get("url") or "(no url returned)"
    print(f'create_card.py: created card "{args.title}" {url}')


if __name__ == "__main__":
    main()
