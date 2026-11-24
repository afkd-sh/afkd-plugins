#!/usr/bin/env python3
# delete_comment.py — `trello.delete_comment`: a comment deleted.
#
# It deletes the comment COMMENT (its id, from `comments.py`) and prints `null`. Trello lets a
# token delete only its own comments: another member's is refused with Trello's own words. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a comment.")
    parser.add_argument("comment", help="the comment's id")
    args = parser.parse_args()
    trello.request("delete comment", "DELETE", f"/actions/{trello.seg(args.comment)}")
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
