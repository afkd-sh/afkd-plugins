#!/usr/bin/env python3
# remove_item.py — `afkd_board.remove_item`: a checklist item removed, the later ones moving up.
#
# It removes the item at INDEX (from 0) of CARD's checklist and prints the card as it is now. The
# change is shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Remove a checklist item.")
    api.add_card_arg(parser)
    parser.add_argument("index", type=int, metavar="INDEX", help="the item's place, from 0")
    args = parser.parse_args()
    board = api.board()
    made = api.request(
        "DELETE", f"/cards/{args.card}/checklist/{args.index}", body=api.with_run({})
    )
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
