#!/usr/bin/env python3
# remove_blocked_by.py — `afkd_board.remove_blocked_by`: a card's block by another taken off.
#
# It takes OTHER's block off CARD and prints CARD as it is now. The change is shown under the run
# when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a card's block by another off.")
    api.add_card_arg(parser)
    parser.add_argument(
        "other", type=int, metavar="OTHER", help="the number of the card that blocks it"
    )
    args = parser.parse_args()
    board = api.board()
    made = api.request(
        "DELETE", f"/cards/{args.card}/blocked_by/{args.other}", body=api.with_run({})
    )
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
