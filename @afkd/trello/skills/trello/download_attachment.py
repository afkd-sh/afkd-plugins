#!/usr/bin/env python3
# download_attachment.py — `trello.download_attachment`: a file on a card saved to a path.
#
# It writes the attachment ATTACHMENT (its id, from `attachments.py`) of the card to TO, an
# absolute path, through a temp file renamed into place so TO never holds part of a file, and
# prints `null`. It refuses a link, which is no file Trello holds, a file over 10 MiB, and a
# relative TO (before any request). The card is --card, else $TRELLO_CARD_ID.
#
# TWO REQUESTS, TWO AUTH SHAPES: the attachment is read with the credential in the query, an
# ordinary read, and its `url` is then fetched verbatim with the OAuth `Authorization` header
# and no query credential, which Trello's download host refuses. It reads TRELLO_API_KEY and
# TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Save a card's attached file to a path.")
    parser.add_argument("attachment", help="the attachment's id")
    parser.add_argument("to", help="the absolute path to write")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    if not os.path.isabs(args.to):
        raise trello.TrelloError(f"to must be absolute, not {trello.quoted(args.to)}")
    card = trello.card_ref(args)
    try:
        found = trello.request(
            "read attachment",
            "GET",
            f"/cards/{trello.seg(card)}/attachments/{trello.seg(args.attachment)}",
            [("fields", trello.ATTACHMENT_FIELDS)],
        )
    except trello.TrelloError as e:
        if "returned status 404" in str(e):
            raise trello.TrelloError(
                f"no attachment {args.attachment} on card {card}: {e}"
            ) from None
        raise
    built = trello.attachment_handle(found, card, "")
    if built is None:
        raise trello.undecodable("read attachment", "no id")
    fields = built["fields"]
    if not fields["is_upload"]:
        raise trello.TrelloError(
            f"attachment {trello.quoted(fields['name'])} is a link to {fields['url']}, "
            "not a file Trello holds"
        )
    size = fields["bytes"]
    if size is not None and size > trello.MAX_ATTACHMENT_BYTES:
        raise trello.TrelloError(
            f"attachment {trello.quoted(fields['name'])} is {size} bytes, over the "
            f"{trello.MAX_ATTACHMENT_BYTES // (1024 * 1024)} MiB the plugin fetches"
        )
    content = trello.download(fields["url"])
    try:
        trello.write_atomic(args.to, content)
    except OSError as e:
        raise trello.TrelloError(f"cannot write {args.to}: {e}") from None
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
