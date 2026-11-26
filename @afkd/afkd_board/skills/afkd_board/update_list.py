#!/usr/bin/env python3
# update_list.py — `afkd_board.update_list`: a list renamed, moved, or both.
#
# It changes the list LIST (by name): --name renames it, --at top|bottom or --after (the name of
# a list) moves it. It prints the list as `{"id", "name"}`. A list's change takes no run.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Rename or move a list.")
    parser.add_argument("list", metavar="LIST")
    parser.add_argument("--name", metavar="NAME")
    api.add_place_args(parser, "the name of the list it goes right after", after_type=str)
    args = parser.parse_args()
    board = api.board()
    id_ = api.list_id(board, args.list)
    body = {}
    if args.name is not None:
        body["name"] = args.name
    if args.after is not None:
        body["after"] = api.list_id(board, args.after)
    elif args.at is not None:
        body["at"] = args.at
    made = api.request("PATCH", f"/lists/{api.seg(id_)}", body=body)
    api.emit({"id": made["id"], "name": made["name"]})


if __name__ == "__main__":
    api.run(main)
