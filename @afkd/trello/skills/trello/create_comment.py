#!/usr/bin/env python3
# create_comment.py — `trello.create_comment`: a comment posted on a card.
#
# It posts BODY (or --body_file's text), markdown, verbatim, on the card, and prints the new
# comment as its `trello.Comment` handle, read back. A comment holds at most 16 384 characters,
# which is checked before any request; anything longer goes on the card as a file with
# `create_attachment.py`. The placeholder is refused (exit 2). The text rides a JSON body, so
# quotes, newlines and unicode arrive as written. The card is --card, else $TRELLO_CARD_ID. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def main():
    parser = argparse.ArgumentParser(description="Post a comment on a card.")
    parser.add_argument("body", nargs="?", help="the comment, markdown, verbatim")
    parser.add_argument("--body_file", metavar="PATH", help="the comment, from a file")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    if (args.body is None) == (args.body_file is None):
        parser.error("give the comment as BODY or as --body_file, not both")
    body = args.body if args.body_file is None else trello.read_text(args.body_file)
    if not body.strip():
        parser.error("the comment is empty")
    trello.refuse_placeholder(body, PLACEHOLDER)
    body = trello.within("body", body)
    card = trello.card_ref(args)
    made = trello.request(
        "create comment", "POST", f"/cards/{trello.seg(card)}/actions/comments", body={"text": body}
    )
    id_ = trello.str_of(made, "id")
    if not id_:
        raise trello.undecodable("create comment", "comment has no id")
    trello.emit(trello.read_back("comment", lambda: trello.read_comment(id_)))


if __name__ == "__main__":
    trello.run(main)
