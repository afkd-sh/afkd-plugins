#!/usr/bin/env python3
# add_label.py — add a label to the active Trello card (ADR-0046 §A).
#
# The add half of the label trio in the bundled `trello` skill (its read half is
# list_labels.py, its remove half remove_label.py): it puts one board label onto
# the card this run works on, so the agent can flag a card (e.g. mark a groomed
# card `ready`). The label is named by its id (a positional arg) OR by --name (an
# exact board-label name, disambiguated with --color when several share a name),
# which this helper resolves to an id against the board's labels. Adding a label
# already on the card is a no-op success, not a duplicate. On a failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# This is the agent-invoked (skill) counterpart to the trello TRIGGER's
# LifecycleAction::AddLabel (crates/trello/src/settings.rs), which afkd fires on
# claim/done/fail; that lifecycle action is untouched here.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
# The board is resolved from the card (its read returns idBoard alongside the
# idLabels needed to detect the no-op), not from $TRELLO_BOARD_ID.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# calls at a local stub listener, mirroring the seam in check_item.py.

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


def post(base, path, query):
    """POST `path?query` with an empty body; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("POST", f"{path}?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def resolve_label_id(base, board, creds, name, color):
    """Resolve --name (with optional --color) to a single board-label id.

    Returns the id on a unique match; prints a clear error and exits non-zero when
    the name matches no board label or more than one (suggesting --color / the id).
    """
    query = urllib.parse.urlencode(creds)
    status, body = get(base, f"/1/boards/{board}/labels", query)
    if not 200 <= status < 300:
        print(
            f"add_label.py: failed to read labels for board {board}",
            file=sys.stderr,
        )
        sys.exit(1)
    labels = json.loads(body)
    matches = [
        label
        for label in labels
        if label.get("name") == name and (color is None or label.get("color") == color)
    ]
    if not matches:
        hint = f" with color {color}" if color is not None else ""
        print(
            f"add_label.py: no board label named {name!r}{hint}",
            file=sys.stderr,
        )
        sys.exit(1)
    if len(matches) > 1:
        colors = ", ".join(sorted(str(m.get("color", "")) for m in matches))
        print(
            f"add_label.py: {name!r} is ambiguous — {len(matches)} labels match "
            f"(colors: {colors}); pass --color or the label id",
            file=sys.stderr,
        )
        sys.exit(1)
    return matches[0].get("id")


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not error on a missing positional; the hand-rolled
    # guard below enforces "exactly one of label_id / --name" with exit 2.
    parser.add_argument("label_id", nargs="?", help="the label id to add")
    parser.add_argument("--name", help="a board label's name to resolve to an id")
    parser.add_argument("--color", help="disambiguate --name when a name repeats")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- Selection guard: exactly one of the positional id / --name; --color only
    #     disambiguates --name. ---
    if bool(args.label_id) == bool(args.name):
        print(
            "add_label.py: pass exactly one of a label id or --name",
            file=sys.stderr,
        )
        sys.exit(2)
    if args.color and not args.name:
        print(
            "add_label.py: --color is only meaningful with --name",
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
            f"add_label.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "add_label.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")
    creds = {"key": key, "token": token}

    # --- Read the card: its idBoard (to resolve --name) and its current idLabels
    #     (to detect the already-present no-op). ---
    query = urllib.parse.urlencode({"fields": "idBoard,idLabels", **creds})
    status, body = get(base, f"/1/cards/{card}", query)
    if not 200 <= status < 300:
        print(
            f"add_label.py: failed to read card {card}",
            file=sys.stderr,
        )
        sys.exit(1)
    card_obj = json.loads(body)
    on_card = set(card_obj.get("idLabels") or [])

    # --- Resolve the target label id: the positional id verbatim, else --name
    #     against the board's labels (0 → unknown, >1 → ambiguous; both non-zero,
    #     no mutation). ---
    if args.label_id:
        label_id = args.label_id
    else:
        label_id = resolve_label_id(
            base, card_obj.get("idBoard"), creds, args.name, args.color
        )

    # --- Already on the card → no-op success, no POST (so it never duplicates). ---
    if label_id in on_card:
        print(f"add_label.py: {label_id} already on card {card}")
        sys.exit(0)

    # --- Add the label. `value` rides the query with key/token; a non-2xx is a
    #     non-zero exit the agent surfaces; the token is never echoed. ---
    query = urllib.parse.urlencode({"value": label_id, **creds})
    status, _ = post(base, f"/1/cards/{card}/idLabels", query)
    if not 200 <= status < 300:
        print(
            f"add_label.py: failed to add {label_id} to card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"add_label.py: added {label_id} to card {card}")


if __name__ == "__main__":
    main()
