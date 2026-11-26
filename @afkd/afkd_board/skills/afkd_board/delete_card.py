#!/usr/bin/env python3
# delete_card.py — `afkd_board.delete_card`: a card deleted for good, with its checklist, its
# comments and its activity.
#
# It prints the card as it was read before it was deleted. The board's API takes no run to delete
# a card, so the change is not shown under one.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a card for good.")
    api.add_card_arg(parser)
    args = parser.parse_args()
    board = api.board()
    read = api.read_card(args.card)
    api.request("DELETE", f"/cards/{args.card}")
    api.emit(api.card_handle(board["id"], read))


if __name__ == "__main__":
    api.run(main)
