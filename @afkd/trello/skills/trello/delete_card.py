#!/usr/bin/env python3
# delete_card.py — `trello.delete_card`: a card deleted for good.
#
# It deletes the card, with its comments, checklists and attachments, and prints `null`.
# Unlike `archive_card.py` there is no way back. The card is --card, else $TRELLO_CARD_ID. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a card for good.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    trello.request("delete card", "DELETE", f"/cards/{trello.seg(trello.card_ref(args))}")
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
