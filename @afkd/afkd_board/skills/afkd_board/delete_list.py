#!/usr/bin/env python3
# delete_list.py — `afkd_board.delete_list`: a list deleted with its archived cards.
#
# It deletes the list LIST (by name) and prints `null`. A list holding cards that are not
# archived is refused, naming how many.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a list.")
    parser.add_argument("list", metavar="LIST")
    args = parser.parse_args()
    board = api.board()
    api.request("DELETE", f"/lists/{api.seg(api.list_id(board, args.list))}")
    api.emit(None)


if __name__ == "__main__":
    api.run(main)
