#!/usr/bin/env python3
# archive_card.py — `trello.archive_card`: a card archived, so it leaves every list.
#
# It archives the card and prints it as its `trello.Card` handle, read back, its `archived`
# true. An archived card shows in no list read, so the card is --card, else $TRELLO_CARD_ID,
# and `unarchive_card.py` brings it back by the id you already know. It reads TRELLO_API_KEY
# and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Archive a card.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    trello.request("archive card", "PUT", f"/cards/{trello.seg(card)}", body={"closed": True})
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
