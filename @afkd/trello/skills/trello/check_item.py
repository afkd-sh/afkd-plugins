#!/usr/bin/env python3
# check_item.py — tick a checklist item on the active Trello card (ADR-0046).
#
# The tick half of the checklist pair (its read half is list_checklists.py): it
# flips one checklist item's complete state on the card this run works on, via the
# Trello REST API, so the agent can mark an item it has GENUINELY completed. The
# checklist is the card's acceptance contract, not a scratchpad — tick only what
# you truly finished. The agent calls it through SKILL.md with the checkItemId as
# the sole argument; on a failure it exits non-zero so the agent surfaces the
# failure rather than retrying blindly.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# It does NOT read its own path from the env (CLAUDE_PLUGIN_ROOT is not exported
# into Bash-tool subprocesses — ADR-0017 Revision §5); SKILL.md passes the path on
# the command line, so this helper owns only the API mechanics.
#
# Idempotent by construction: it does not pre-read state or special-case a no-op —
# a repeat identical PUT (setting an already-set state) simply returns 2xx.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the PUT
# at a local stub listener, mirroring the seam in list_checklists.py.

import argparse
import http.client
import os
import sys
import urllib.parse


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def set_state(base, card, item, state, key, token):
    """PUT the item's `state` on the card; return (status, body-bytes).

    The state/key/token ride in the query string with an empty body, matching the
    query-param auth the trigger's REST client uses (crates/trello/src/client.rs).
    The token is never echoed.
    """
    query = urllib.parse.urlencode({"state": state, "key": key, "token": token})
    conn = connection(base)
    try:
        conn.request("PUT", f"/1/cards/{card}/checkItem/{item}?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not emit its own error on a missing id; the
    # hand-rolled guard below matches post_comment.py's message style and exit 2.
    parser.add_argument("checkItemId", nargs="?")
    parser.add_argument(
        "--uncheck",
        action="store_true",
        help="mark the item incomplete instead of complete",
    )
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- The checkItemId: the single argument. ---
    if not args.checkItemId:
        print(
            "check_item.py: a checkItemId is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    item = args.checkItemId

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
            f"check_item.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "check_item.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    state = "incomplete" if args.uncheck else "complete"

    # --- Set the item's state. A non-2xx (e.g. an auth failure or a bad id) is a
    #     non-zero exit the agent surfaces; the token is never echoed. ---
    status, _ = set_state(base, card, item, state, key, token)
    if not 200 <= status < 300:
        print(
            f"check_item.py: failed to mark {item} {state} on card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"check_item.py: marked {item} {state} on card {card}")


if __name__ == "__main__":
    main()
