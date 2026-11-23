#!/usr/bin/env python3
# update_check_item.py — `trello.update_check_item`: a check item renamed, ticked or unticked.
#
# It changes the check item ITEM (its id, from `check_items.py`) of the card as the flags say,
# in one `PUT`: --name renames it, --done ticks it, --undone unticks it. Tick only what you
# have genuinely completed. With none of them it sends nothing. It prints the item as its
# `trello.Check_Item` handle, read back. The card is --card, else $TRELLO_CARD_ID. It reads
# TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Change a check item.")
    parser.add_argument("item", help="the check item's id")
    trello.add_card_arg(parser)
    parser.add_argument("--name", help="the item's new name")
    state = parser.add_mutually_exclusive_group()
    state.add_argument("--done", action="store_true", help="tick it")
    state.add_argument("--undone", action="store_true", help="untick it")
    args = parser.parse_args()
    card_id, _ = trello.card_ids(trello.card_ref(args))
    body = {}
    if args.name is not None:
        body["name"] = args.name
    if args.done or args.undone:
        body["state"] = "complete" if args.done else "incomplete"
    if body:
        trello.request(
            "update check item",
            "PUT",
            f"/cards/{trello.seg(card_id)}/checkItem/{trello.seg(args.item)}",
            body=body,
        )
    trello.emit(trello.read_back("check item", lambda: trello.read_check_item(card_id, args.item)))


if __name__ == "__main__":
    trello.run(main)
