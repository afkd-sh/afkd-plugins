#!/usr/bin/env python3
# check_items.py — `trello.check_items`: every check item of a card, checklist by checklist.
#
# It prints each check item's handle as JSON, an array: its `id`, its `key`
# (`trello:<card id>/<item id>`), and its `card` (the card's short link), `checklist` (its
# checklist's id), `name` and `done`. The card is --card, else $TRELLO_CARD_ID. It reads
# TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A card's check items.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card_id, short = trello.card_ids(trello.card_ref(args))
    items = []
    for lst in trello.checklists_of(card_id):
        items.extend(trello.check_item_handles(lst, card_id, short))
    trello.emit(items)


if __name__ == "__main__":
    trello.run(main)
