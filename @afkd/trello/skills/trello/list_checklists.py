#!/usr/bin/env python3
# list_checklists.py — re-read the active Trello card's checklists (ADR-0046).
#
# The read half of the checklist pair (its tick half is check_item.py): it lists
# the checklists on the card this run works on, via the Trello REST API, and
# prints each checklist grouped with one line per item carrying the item's
# checkItemId and its [x]/[ ] state. The standing brief already shows the
# checklist items and their ids; this helper is the on-demand re-reader for
# current state. The agent calls it through SKILL.md with no argument (it reads
# everything from the environment); on a fetch or auth failure it exits non-zero
# so the agent surfaces the failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# It does NOT read its own path from the env (CLAUDE_PLUGIN_ROOT is not exported
# into Bash-tool subprocesses — ADR-0017 Revision §5); SKILL.md passes the path on
# the command line, so this helper owns only the API mechanics.
#
# It parses the checklists array with the `json` stdlib module (no jq): item text
# is arbitrary (quotes, unicode), so a hand-parse would corrupt it.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# fetch at a local stub listener, mirroring the TrelloClient::with_base testing
# seam (crates/trello/src/client.rs).

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


def get_checklists(base, card, query):
    """GET the card's checklists (items nested) with `query`; return (status, body)."""
    conn = connection(base)
    try:
        conn.request("GET", f"/1/cards/{card}/checklists?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(checklist):
    """One block per checklist: a `### <name> (done/total)` header, then its items.

    Each item is `<checkItemId>  [x]  <name>` (`[ ]` when not complete) so the id
    (the handle check_item.py acts on) leads every line and the state is visible.
    """
    items = checklist.get("checkItems") or []
    done = sum(1 for i in items if i.get("state") == "complete")
    lines = [f"### {checklist.get('name', '')} ({done}/{len(items)})"]
    for item in items:
        box = "[x]" if item.get("state") == "complete" else "[ ]"
        lines.append(f"{item.get('id')}  {box}  {item.get('name', '')}")
    return "\n".join(lines)


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
            f"list_checklists.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "list_checklists.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Fetch the card's checklists with items nested. A non-2xx (e.g. an auth
    #     failure) is a non-zero exit the agent surfaces; the token is never
    #     echoed. ---
    query = urllib.parse.urlencode(
        {"checkItems": "all", "key": key, "token": token}
    )
    status, body = get_checklists(base, card, query)
    if not 200 <= status < 300:
        print(
            f"list_checklists.py: failed to read checklists for card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Trello returns a top-level array of checklists; an empty card is
    #     success, not an error. ---
    checklists = json.loads(body)
    if not checklists:
        print(f"no checklists on card {card}")
        sys.exit(0)

    print("\n\n".join(render(c) for c in checklists))


if __name__ == "__main__":
    main()
