#!/usr/bin/env python3
# move_card.py — `afkd_board.move_card`: a card moved to a list.
#
# It moves CARD to LIST (by name, matched exactly), at the bottom unless --at top|bottom or
# --after (the number of a card in LIST) says, and prints the card as it is now. The change is
# shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Move a card to a list.")
    api.add_card_arg(parser)
    parser.add_argument("list", metavar="LIST")
    api.add_place_args(parser, "the number of the card in LIST it goes right after")
    args = parser.parse_args()
    board = api.board()
    api.list_id(board, args.list)
    body = {"list": args.list, **(api.place_of(args) or {"at": "bottom"})}
    made = api.request("PATCH", f"/cards/{args.card}", body=api.with_run(body))
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
