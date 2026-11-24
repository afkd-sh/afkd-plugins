#!/usr/bin/env python3
# delete_check_item.py — `trello.delete_check_item`: a check item deleted.
#
# It deletes the check item ITEM (its id, from `check_items.py`) of the card and prints
# `null`. The card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a check item.")
    parser.add_argument("item", help="the check item's id")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    trello.request(
        "delete check item",
        "DELETE",
        f"/cards/{trello.seg(card)}/checkItem/{trello.seg(args.item)}",
    )
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
