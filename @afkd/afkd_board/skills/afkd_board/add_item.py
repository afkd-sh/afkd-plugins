#!/usr/bin/env python3
# add_item.py — `afkd_board.add_item`: an item added to the end of a card's checklist, not ticked.
#
# It prints the card as it is now. The change is shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Add an item to a card's checklist.")
    api.add_card_arg(parser)
    parser.add_argument("text", metavar="TEXT")
    args = parser.parse_args()
    board = api.board()
    made = api.request(
        "POST", f"/cards/{args.card}/checklist", body=api.with_run({"text": args.text})
    )
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
