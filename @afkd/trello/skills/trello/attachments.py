#!/usr/bin/env python3
# attachments.py — `trello.attachments`: a card's attachments, oldest first.
#
# It prints each attachment's handle as JSON, an array: its `id`, its `key`
# (`trello:<card id>/<attachment id>`), and its `card` (the card's short link), `name`, `url`,
# `bytes` (null for a link), `mime_type`, `created_at` and `is_upload` (a file Trello holds,
# not a link). A screenshot a human pasted into a comment is one. The card is --card, else
# $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A card's attachments.")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card_id, short = trello.card_ids(trello.card_ref(args))
    listed = trello.get_array(
        "read attachments",
        f"/cards/{trello.seg(card_id)}/attachments",
        [("fields", trello.ATTACHMENT_FIELDS)],
    )
    built = trello.only_handles(trello.attachment_handle(a, card_id, short) for a in listed)
    built.sort(key=lambda h: (h["fields"]["created_at"], h["id"]))
    trello.emit(built)


if __name__ == "__main__":
    trello.run(main)
