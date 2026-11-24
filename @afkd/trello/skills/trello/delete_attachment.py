#!/usr/bin/env python3
# delete_attachment.py — `trello.delete_attachment`: an attachment taken off a card.
#
# It deletes the attachment ATTACHMENT (its id, from `attachments.py`) of the card and prints
# `null`. The card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take an attachment off a card.")
    parser.add_argument("attachment", help="the attachment's id")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    trello.request(
        "delete attachment",
        "DELETE",
        f"/cards/{trello.seg(card)}/attachments/{trello.seg(args.attachment)}",
    )
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
