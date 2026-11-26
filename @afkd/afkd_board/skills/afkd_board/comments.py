#!/usr/bin/env python3
# comments.py — `afkd_board.comments`: a card's comments, oldest first.
#
# It prints an array of comment handles: `id` the comment's uuid, `key`, and the fields `card`
# (the card's number), `body`, `author` (the person's name; null for a comment a key made, and
# once the person's account is deleted), `run` (the id of the run it was made in, else null),
# `created_at` and `url` (its card's).

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Read a card's comments.")
    api.add_card_arg(parser)
    args = parser.parse_args()
    board = api.board()
    card = api.read_card(args.card)
    comments = api.request("GET", f"/cards/{args.card}/comments")
    api.emit([api.comment_handle(board["id"], card, c) for c in comments])


if __name__ == "__main__":
    api.run(main)
