#!/usr/bin/env python3
# lists.py — `afkd_board.lists`: the board's lists, in order.
#
# It prints an array of `{"id", "name"}`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    argparse.ArgumentParser(description="Read the board's lists.").parse_args()
    api.emit([{"id": l["id"], "name": l["name"]} for l in api.board()["lists"]])


if __name__ == "__main__":
    api.run(main)
