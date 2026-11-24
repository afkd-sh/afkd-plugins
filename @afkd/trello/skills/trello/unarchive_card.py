#!/usr/bin/env python3
# unarchive_card.py — `trello.unarchive_card`: an archived card put back on its board.
#
# It unarchives the card and prints it as its `trello.Card` handle, read back, its `archived`
# false. The card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Unarchive a card.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    trello.request("unarchive card", "PUT", f"/cards/{trello.seg(card)}", body={"closed": False})
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
