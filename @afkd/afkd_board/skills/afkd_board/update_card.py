#!/usr/bin/env python3
# update_card.py — `afkd_board.update_card`: a card's title or description changed.
#
# It prints the card as it is now. Give --title, the description (--description's text or
# --description_file's, markdown), or both. The change is shown under the run when the run is
# known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Change a card's title or description.")
    api.add_card_arg(parser)
    parser.add_argument("--title", metavar="TEXT")
    api.add_description_args(parser)
    args = parser.parse_args()
    description = api.description_of(args)
    if args.title is None and description is None:
        parser.error("give --title, a description, or both")
    body = {}
    if args.title is not None:
        body["title"] = args.title
    if description is not None:
        body["description"] = description
    board = api.board()
    made = api.request("PATCH", f"/cards/{args.card}", body=api.with_run(body))
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
