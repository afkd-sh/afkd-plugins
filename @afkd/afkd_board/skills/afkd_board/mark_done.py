#!/usr/bin/env python3
# mark_done.py — `afkd_board.mark_done`: a card marked done, or not.
#
# It marks CARD done, or with --done false not, and prints the card as it is now. A blocked card
# is refused, naming the cards that are not done. The change is shown under the run when the run
# is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Mark a card done, or not.")
    api.add_card_arg(parser)
    parser.add_argument("--done", type=api.truth, default=True, metavar="true|false")
    args = parser.parse_args()
    board = api.board()
    made = api.request("PATCH", f"/cards/{args.card}", body=api.with_run({"done": args.done}))
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
