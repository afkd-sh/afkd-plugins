#!/usr/bin/env python3
# lists.py — `trello.lists`: a board's open lists, in the board's order.
#
# It prints each list's handle as JSON, an array: its `id`, its `key`, and its `name`,
# `board` (the board's short link) and `archived`. The board is --board, else
# $TRELLO_BOARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A board's open lists.")
    trello.add_board_arg(parser)
    args = parser.parse_args()
    board_id, short = trello.read_board(trello.board_ref(args))
    lists = trello.get_array(
        "read lists",
        f"/boards/{trello.seg(board_id)}/lists",
        [("filter", "open"), ("fields", "name,closed,pos")],
    )
    trello.emit(
        trello.only_handles(trello.list_handle(item, short) for item in trello.by_position(lists))
    )


if __name__ == "__main__":
    trello.run(main)
