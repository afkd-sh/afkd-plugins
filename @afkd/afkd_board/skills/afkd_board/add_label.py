#!/usr/bin/env python3
# add_label.py — `afkd_board.add_label`: a label put on a card.
#
# It puts LABEL (by name; one the board lacks is made in the next free colour) on CARD and prints
# the card as it is now. `afkd/claimed` is the service's: leave it alone. The change is shown
# under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a label on a card.")
    api.add_card_arg(parser)
    parser.add_argument("label", metavar="LABEL")
    args = parser.parse_args()
    board = api.board()
    made = api.request(
        "POST", f"/cards/{args.card}/labels/{api.seg(args.label)}", body=api.with_run({})
    )
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
