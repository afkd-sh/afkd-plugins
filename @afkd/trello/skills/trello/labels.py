#!/usr/bin/env python3
# labels.py — `trello.labels`: a board's labels, every one of them, named or not.
#
# It prints each label's handle as JSON, an array: its `id`, its `key`, and its `name` ("" for
# a colour-only label), `color` (null for none) and `board` (the board's short link). The
# board is --board, else $TRELLO_BOARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A board's labels.")
    trello.add_board_arg(parser)
    args = parser.parse_args()
    board_id, short = trello.read_board(trello.board_ref(args))
    labels = trello.board_labels(board_id)
    trello.emit(trello.only_handles(trello.label_handle(label, short) for label in labels))


if __name__ == "__main__":
    trello.run(main)
