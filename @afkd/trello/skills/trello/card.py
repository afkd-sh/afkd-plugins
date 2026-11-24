#!/usr/bin/env python3
# card.py — a card read whole, as the `trello.Card` handle it is now.
#
# It prints the card's handle as JSON: its `id` the short link, its `key`, and its fields
# (`number`, `name`, `url`, `description`, `board`, `list`, `labels`, `members`, `due`,
# `due_complete`, `archived`, `created_at`, `last_activity_at`), the description verbatim. The
# card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Read a card whole.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    trello.emit(trello.read_card(trello.card_ref(args)))


if __name__ == "__main__":
    trello.run(main)
