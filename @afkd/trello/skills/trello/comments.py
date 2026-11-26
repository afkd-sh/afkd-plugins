#!/usr/bin/env python3
# comments.py — `trello.comments`: a card's comments, oldest first.
#
# It prints the newest --limit comments of the card (default 200, at most 1000), each as its
# `trello.Comment` handle, an array: its `id`, its `key`, and its `card` (the card's short
# link), `body` (verbatim), `author` (a username, null for none), `created_at` and
# `edited_at` (unix milliseconds, null until edited). afkd's own markers, such as
# `[afkd-claim]`, are comments among them. The card is --card, else $TRELLO_CARD_ID. It reads
# TRELLO_API_KEY and TRELLO_TOKEN. Ports src/reads.rs's `comment_actions`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def limit_arg(text):
    """An argparse type: a count from 1 to 1000."""
    try:
        limit = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is no whole number") from None
    if not 1 <= limit <= trello.MAX_LIMIT:
        raise argparse.ArgumentTypeError(f"limit {limit} is outside 1 to {trello.MAX_LIMIT}")
    return limit


def comment_actions(card_id, limit):
    """The newest `limit` comment actions of the card, oldest first, paged with Trello's
    `before`: each page the newest the action log has below the oldest the last one held."""
    path = f"/cards/{trello.seg(card_id)}/actions"
    actions, before = [], None
    while len(actions) < limit:
        want = min(limit - len(actions), trello.MAX_LIMIT)
        pairs = [
            ("filter", "commentCard"),
            ("memberCreator_fields", "username,fullName"),
            ("limit", str(want)),
        ]
        if before is not None:
            pairs.append(("before", before))
        page = trello.get_array("read comments", path, pairs)
        ids = [a["id"] for a in page if isinstance(a, dict) and isinstance(a.get("id"), str)]
        actions.extend(page)
        if not (ids and len(page) >= want):
            break
        before = min(ids)
    unique, last = [], None
    for action in sorted(actions, key=lambda a: trello.str_of(a, "id")):
        if trello.str_of(action, "id") != last:
            unique.append(action)
            last = trello.str_of(action, "id")
    return unique[max(len(unique) - limit, 0) :]


def main():
    parser = argparse.ArgumentParser(description="A card's comments, oldest first.")
    trello.add_card_arg(parser)
    parser.add_argument("--limit", type=limit_arg, default=200, help="1 to 1000 (default 200)")
    args = parser.parse_args()
    card_id, short = trello.card_ids(trello.card_ref(args))
    actions = comment_actions(card_id, args.limit)
    trello.emit(trello.only_handles(trello.comment_handle(a, short) for a in actions))


if __name__ == "__main__":
    trello.run(main)
