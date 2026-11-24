#!/usr/bin/env python3
# checklists.py — `trello.checklists`: a card's checklists, in the card's order.
#
# It prints each checklist's handle as JSON, an array: its `id`, its `key`, and its `card`
# (the card's short link), `name` and `items` (the names of its check items, in order). Its
# check items themselves are `check_items.py`'s. The card is --card, else $TRELLO_CARD_ID. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A card's checklists.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card_id, short = trello.card_ids(trello.card_ref(args))
    lists = trello.checklists_of(card_id)
    trello.emit(trello.only_handles(trello.checklist_handle(lst, short) for lst in lists))


if __name__ == "__main__":
    trello.run(main)
