#!/usr/bin/env python3
# boards.py — `trello.boards`: the open boards the credential reaches.
#
# It prints each board's handle as JSON, an array: its `id` the short link, its `key`, and
# its `name`, `url` and `archived`. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    argparse.ArgumentParser(description="The boards this run reaches.").parse_args()
    boards = trello.get_array(
        "read boards",
        "/members/me/boards",
        [("filter", "open"), ("fields", "name,url,shortLink,closed")],
    )
    trello.emit(trello.only_handles(trello.board_handle(b) for b in boards))


if __name__ == "__main__":
    trello.run(main)
