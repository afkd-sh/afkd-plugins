#!/usr/bin/env python3
# update_comment.py — `trello.update_comment`: a comment's text replaced.
#
# It replaces the text of the comment COMMENT (its id, from `comments.py`) with BODY (or
# --body_file's text), markdown, verbatim, through `PUT /actions/{id}`, and prints the comment
# as its `trello.Comment` handle, read back, its `edited_at` set. Trello lets a token edit
# only its own comments: another member's is refused with Trello's own words. At most 16 384
# characters, checked before any request; the placeholder is refused (exit 2). It reads
# TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def main():
    parser = argparse.ArgumentParser(description="Replace a comment's text.")
    parser.add_argument("comment", help="the comment's id")
    parser.add_argument("body", nargs="?", help="the new text, markdown, verbatim")
    parser.add_argument("--body_file", metavar="PATH", help="the new text, from a file")
    args = parser.parse_args()
    if (args.body is None) == (args.body_file is None):
        parser.error("give the text as BODY or as --body_file, not both")
    body = args.body if args.body_file is None else trello.read_text(args.body_file)
    if not body.strip():
        parser.error("the comment is empty")
    trello.refuse_placeholder(body, PLACEHOLDER)
    body = trello.within("body", body)
    trello.request(
        "update comment", "PUT", f"/actions/{trello.seg(args.comment)}", body={"text": body}
    )
    trello.emit(trello.read_back("comment", lambda: trello.read_comment(args.comment)))


if __name__ == "__main__":
    trello.run(main)
