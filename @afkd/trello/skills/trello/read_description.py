#!/usr/bin/env python3
# read_description.py — print the active Trello card's description (ADR-0046 §A).
#
# The read half of the card-description pair in the bundled `trello` skill (its
# write half is set_description.py): it fetches the `desc` field of the card this
# run works on and prints it verbatim, so an agent can read the current description,
# change it, and write the whole of it back with set_description.py. On a fetch or
# auth failure it exits non-zero so the agent surfaces the failure rather than
# retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# fetch at a local stub listener, mirroring the seam in list_comments.py.

import argparse
import base64
import http.client
import json
import os
import sys
import urllib.parse
import urllib.request


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`, tunnelled through
    the proxy the environment names for its scheme, minus the hosts NO_PROXY lists: a sandbox
    with a network list reaches its names only through afkd's proxy (ADR-0084)."""
    parts = urllib.parse.urlsplit(base)
    https = parts.scheme == "https"
    kind = http.client.HTTPSConnection if https else http.client.HTTPConnection
    proxy = urllib.request.getproxies().get(parts.scheme)
    if not proxy or urllib.request.proxy_bypass(parts.hostname):
        return kind(parts.hostname, parts.port)
    via = urllib.parse.urlsplit(proxy)
    conn = kind(via.hostname, via.port)
    headers = {}
    if via.username is not None:
        user = urllib.parse.unquote(via.username)
        password = urllib.parse.unquote(via.password or "")
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        headers["Proxy-Authorization"] = f"Basic {token}"
    conn.set_tunnel(parts.hostname, parts.port or (443 if https else 80), headers=headers)
    return conn


def get_description(base, card, query):
    """GET only the card's `desc` field with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/cards/{card}?{query}")
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
            f"read_description.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "read_description.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the description. `fields=desc` keeps the response to the one field
    #     we print; a non-2xx (e.g. an auth failure) is a non-zero exit the agent
    #     surfaces. The token is never echoed. ---
    query = urllib.parse.urlencode({"fields": "desc", "key": key, "token": token})
    status, body = get_description(base, card, query)
    if not 200 <= status < 300:
        print(
            f"read_description.py: failed to read the description of card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns the card object; print its `desc` verbatim (an empty
    #     description prints an empty line, which is success). ---
    card_obj = json.loads(body)
    print(card_obj.get("desc", ""))


if __name__ == "__main__":
    main()
