#!/usr/bin/env python3
# checklist.py — `afkd_board.checklist`: a card's checklist, its items in the card's order.
#
# It prints an array of `{"text", "done"}`, empty when the card has no checklist.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Read a card's checklist.")
    api.add_card_arg(parser)
    args = parser.parse_args()
    card = api.read_card(args.card)
    api.emit([{"text": i["text"], "done": i["done"]} for i in card["checklist"]])


if __name__ == "__main__":
    api.run(main)
