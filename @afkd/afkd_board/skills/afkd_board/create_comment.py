#!/usr/bin/env python3
# create_comment.py — `afkd_board.create_comment`: a comment made on a card.
#
# It makes BODY (or --body_file's text), markdown, 1 to 10 000 characters, verbatim, on CARD, and
# prints it as a comment handle. The placeholder is refused (exit 2). The text rides a JSON body,
# so quotes, newlines and unicode arrive as written. The change is shown under the run when the
# run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def main():
    parser = argparse.ArgumentParser(description="Make a comment on a card.")
    api.add_card_arg(parser)
    parser.add_argument("body", nargs="?", help="the comment, markdown, verbatim")
    parser.add_argument("--body_file", metavar="PATH", help="the comment, from a file")
    args = parser.parse_args()
    if (args.body is None) == (args.body_file is None):
        parser.error("give the comment as BODY or as --body_file, not both")
    body = args.body if args.body_file is None else api.read_text(args.body_file)
    if not body.strip():
        parser.error("the comment is empty")
    if body == PLACEHOLDER:
        api.die("refusing to send the placeholder text; substitute your own", 2)
    board = api.board()
    card = api.read_card(args.card)
    made = api.request("POST", f"/cards/{args.card}/comments", body=api.with_run({"body": body}))
    api.emit(api.comment_handle(board["id"], card, made))


if __name__ == "__main__":
    api.run(main)
