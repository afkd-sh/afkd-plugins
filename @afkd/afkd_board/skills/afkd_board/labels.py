#!/usr/bin/env python3
# labels.py — `afkd_board.labels`: the board's labels.
#
# It prints an array of `{"name", "colour"}`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    argparse.ArgumentParser(description="Read the board's labels.").parse_args()
    api.emit([{"name": l["name"], "colour": l["colour"]} for l in api.board()["labels"]])


if __name__ == "__main__":
    api.run(main)
