#!/usr/bin/env python3
# create_card.py — `afkd_board.create_card`: a card with the board's next number.
#
# It makes the card in LIST (by name, matched exactly) titled TITLE, at the bottom unless --at
# top|bottom or --after (the number of a card in LIST) says, and prints it as a card handle. A
# --label the board lacks is made in the next free colour (repeat it for several); each --item is
# a checklist item, none ticked (repeat it). The description is --description's text or
# --description_file's, markdown. The change is shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Make a card.")
    parser.add_argument("list", metavar="LIST")
    parser.add_argument("title", metavar="TITLE")
    api.add_description_args(parser)
    parser.add_argument("--label", action="append", default=[], metavar="NAME")
    parser.add_argument("--item", action="append", default=[], metavar="TEXT")
    api.add_place_args(parser, "the number of the card in LIST it goes right after")
    args = parser.parse_args()
    board = api.board()
    api.list_id(board, args.list)
    body = {"list": args.list, "title": args.title}
    description = api.description_of(args)
    if description is not None:
        body["description"] = description
    if args.label:
        body["labels"] = args.label
    if args.item:
        body["checklist"] = args.item
    body.update(api.place_of(args))
    made = api.request("POST", "/cards", body=api.with_run(body))
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
