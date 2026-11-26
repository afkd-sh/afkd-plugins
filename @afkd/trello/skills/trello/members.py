#!/usr/bin/env python3
# members.py — `trello.members`: a board's members.
#
# It prints each member's handle as JSON, an array: its `id` the username, its `key`, and its
# `full_name`. The board is --board, else $TRELLO_BOARD_ID. It reads TRELLO_API_KEY and
# TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="A board's members.")
    trello.add_board_arg(parser)
    args = parser.parse_args()
    board_id, _ = trello.read_board(trello.board_ref(args))
    trello.emit(trello.only_handles(trello.member_handle(m) for m in trello.board_members(board_id)))


if __name__ == "__main__":
    trello.run(main)
