#!/usr/bin/env python3
# archive_card.py — `afkd_board.archive_card`: a card archived, which leaves its list.
#
# It prints the card as it is now. The change is shown under the run when the run is known.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Archive a card.")
    api.add_card_arg(parser)
    args = parser.parse_args()
    board = api.board()
    made = api.request("POST", f"/cards/{args.card}/archive", body=api.with_run({}))
    api.emit(api.card_handle(board["id"], made))


if __name__ == "__main__":
    api.run(main)
