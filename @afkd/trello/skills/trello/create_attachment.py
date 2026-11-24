#!/usr/bin/env python3
# create_attachment.py — `trello.create_attachment`: a local file put on a card.
#
# It uploads the file at PATH, an absolute path, to the card as --name (the file's own name
# when none is given), and prints the attachment as its `trello.Attachment` handle, read back.
# The file is read before anything is sent, so a relative path, a file that is missing or is
# not a regular file, and one over 10 MiB exit 1 with nothing uploaded. The upload is a
# `multipart/form-data` POST with the credential in the query, which the standard library does
# not build, so the body is assembled by hand. The card is --card, else $TRELLO_CARD_ID. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a file on a card.")
    parser.add_argument("path", help="the file, an absolute path")
    parser.add_argument("--name", help="the attachment's name (default: the file's own)")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    name = args.name if args.name is not None else os.path.basename(args.path)
    upload = trello.read_upload(args.path, name)
    card_id, short = trello.card_ids(trello.card_ref(args))
    made = trello.request(
        "create attachment", "POST", f"/cards/{trello.seg(card_id)}/attachments", upload=upload
    )
    id_ = trello.str_of(made, "id")
    if not id_:
        raise trello.undecodable("create attachment", "attachment has no id")

    def read():
        found = trello.request(
            "read attachment",
            "GET",
            f"/cards/{trello.seg(card_id)}/attachments/{trello.seg(id_)}",
            [("fields", trello.ATTACHMENT_FIELDS)],
        )
        built = trello.attachment_handle(found, card_id, short)
        if built is None:
            raise trello.undecodable("read attachment", "no id")
        return built

    trello.emit(trello.read_back("attachment", read))


if __name__ == "__main__":
    trello.run(main)
