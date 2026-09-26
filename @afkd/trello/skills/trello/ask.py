#!/usr/bin/env python3
# ask.py — ask the human a question on the active Trello card and park the run
# (ADR-0017 §7, ADR-0031 clarification gate).
#
# The "I cannot build this as it stands" action of the bundled `trello` skill (its
# read action is list_comments.py, its post action post_comment.py). In one step it
# (1) posts the agent's message as a comment on the card this run works on, via the
# Trello REST API, and (2) writes the park marker `park` into the run's scratch dir
# ($AFKD_SCRATCH_DIR). A MULTI-step workflow gates on that marker
# (`if run_cmd "test -f $AFKD_SCRATCH_DIR/park" { fail … }`) to stop the run at the
# ask, and the afkd trello trigger reads it to PARK the card instead of failing it:
# it adds the `Awaiting Reply` label, releases the claim, moves nothing else, and
# goes on to the next card. Once a human replies in the thread, a later poll finds
# the badged card wherever it sits, re-claims it, and a fresh run starts with the
# answer in its brief. The marker is written only AFTER the comment lands, so a
# failed post never parks a card with no question on it. Scratch is outside any
# in_worktree copy and fresh per attempt, so agent, gate, and trigger name the same
# file whether or not the run is worktreed, and no marker outlives the attempt that
# wrote it.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger merges into the worker child (crates/trello/src/trigger.rs:
# TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID, and AFKD_SCRATCH_DIR holding the
# per-run scratch dir). The card is --card if given, else $TRELLO_CARD_ID; there is
# no scratch fallback. It does NOT read its own path from the env
# (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses — ADR-0017
# Revision §5); SKILL.md passes the path on the command line, so this helper owns
# only the API + marker mechanics. The marker name must match afkd_forge::PARK_FILE.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the post
# at a local stub listener, mirroring the seam in post_comment.py.

import argparse
import http.client
import os
import sys
import urllib.parse

PLACEHOLDER = "<REPLACE WITH YOUR QUESTIONS>"

# Scratch-relative park marker, written under $AFKD_SCRATCH_DIR. The workflow gates
# on it (`if run_cmd "test -f $AFKD_SCRATCH_DIR/park" { fail … }`) to stop the run
# at the ask, and the trigger reads it to park the card. Must match
# crates/forge/src/lib.rs::PARK_FILE.
PARK_FILE = "park"


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def post_comment(base, card, text, key, token):
    """POST one comment to the card's actions; return (status, body-bytes).

    The same query-string endpoint post_comment.py uses (and the same one the
    trigger's REST client posts to — crates/trello/src/client.rs::post_comment), so
    quotes, newlines and unicode survive url-encoding. The token is never echoed.
    """
    query = urllib.parse.urlencode({"text": text, "key": key, "token": token})
    conn = connection(base)
    try:
        conn.request("POST", f"/1/cards/{card}/actions/comments?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("message", nargs="?")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- The question: the single argument. ---
    if not args.message:
        print(
            "ask.py: a question message is required (pass it as the only argument)",
            file=sys.stderr,
        )
        sys.exit(2)
    text = args.message

    # --- Refuse the SKILL.md placeholder: never post the fill-me-in example. ---
    if text == PLACEHOLDER:
        print(
            "ask.py: refusing to post the placeholder text; substitute your real questions",
            file=sys.stderr,
        )
        sys.exit(2)

    # --- Credentials + the marker's destination: required, from the inherited
    #     environment. Scratch is checked up front, BEFORE the post: a park we
    #     cannot write would leave a question on the card that nothing ever parks,
    #     and the run would then fail as if the agent had never asked. ---
    key = os.environ.get("TRELLO_API_KEY")
    token = os.environ.get("TRELLO_TOKEN")
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    missing = [
        name
        for name, value in (
            ("TRELLO_API_KEY", key),
            ("TRELLO_TOKEN", token),
            ("AFKD_SCRATCH_DIR", scratch),
        )
        if not value
    ]
    if missing:
        print(
            f"ask.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "ask.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Post the question first; only park once it has landed. It carries no afkd
    #     marker: the trigger keys a comment's identity on the posting member id, so
    #     this reads as afkd's own word — which is exactly the line a human's reply
    #     has to be newer than for the card to be re-armed. ---
    status, _ = post_comment(base, card, text, key, token)
    if not 200 <= status < 300:
        print(
            f"ask.py: failed to post the question to card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Write the park marker into the run's scratch dir (must match
    #     afkd_forge::PARK_FILE), which the trigger already created. Never
    #     cwd-relative: under `in_worktree` the cwd is a copy that is torn down
    #     before the trigger reads the marker. ---
    marker = os.path.join(scratch, PARK_FILE)
    try:
        with open(marker, "w", encoding="utf-8"):
            pass
    except OSError as e:
        print(
            f"ask.py: posted the question but could not write the park marker {marker}: {e}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"ask.py: asked card {card} and parked the run awaiting a reply")


if __name__ == "__main__":
    main()
