#!/usr/bin/env python3
# card.py — `afkd_board.card`: a card read whole, as the `afkd_board.Card` handle it is now.
#
# It prints the card's handle as JSON: its `id` the card's number as its digits, its `key`, and its
# fields (`number`, `title`, `description`, `list`, `labels`, `archived`, `done`, `blocked_by`,
# `blocking`, `blocked`, `url`, `created_at`, `updated_at`). The card is archived or not.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Read a card whole.")
    api.add_card_arg(parser)
    args = parser.parse_args()
    board = api.board()
    api.emit(api.card_handle(board["id"], api.read_card(args.card)))


if __name__ == "__main__":
    api.run(main)
