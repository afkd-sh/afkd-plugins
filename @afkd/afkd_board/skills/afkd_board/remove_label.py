#!/usr/bin/env python3
# remove_label.py — `afkd_board.remove_label`: a label taken off a card.
#
# It takes LABEL off CARD and prints the card as it is now. A label the card lacks, or the board
# does, leaves the card as it is, and writes nothing. `afkd/claimed` is the service's: leave it
# alone. The change is shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a label off a card.")
    api.add_card_arg(parser)
    parser.add_argument("label", metavar="LABEL")
    args = parser.parse_args()
    board = api.board()
    if not any(l["name"] == args.label for l in board["labels"]):
        api.emit(api.card_handle(board["id"], api.read_card(args.card)))
        return
    made = api.request(
        "DELETE", f"/cards/{args.card}/labels/{api.seg(args.label)}", body=api.with_run({})
    )
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
