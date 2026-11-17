#!/usr/bin/env python3
# create_comment.py — `linear.create_comment`: a comment posted on an issue.
#
# It posts `body` (or --body_file's text), markdown, verbatim, on the issue, each of --files
# put up and embedded after it: an image shown, any other file linked. With --reply_to it goes
# in that comment's thread, a comment on the same issue; a reply to a reply goes in the thread
# both are in. It prints the new comment's `linear.Comment` handle as JSON, as the plugin's
# `create_comment` gives it back. Each file is read before anything is posted, and one that is
# missing or over 10 MiB exits 1. It refuses SKILL.md's placeholder and an empty comment with
# no file (exit 2) before any request. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def main():
    parser = argparse.ArgumentParser(description="Post a comment on an issue.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    parser.add_argument("body", nargs="?", help="the comment, markdown, verbatim")
    parser.add_argument("--body_file", metavar="PATH", help="the comment, from a file")
    linear.add_file_args(parser)
    parser.add_argument("--reply_to", metavar="COMMENT", help="the id of the comment it answers")
    args = parser.parse_args()
    if (args.body is None) == (args.body_file is None):
        parser.error("give the comment as body or as --body_file, not both")
    body = args.body if args.body_file is None else linear.read_text(args.body_file)
    if not body.strip() and not args.files:
        parser.error("the comment is empty")
    linear.refuse_placeholder(body, PLACEHOLDER)
    linear.emit(linear.create_comment(args.issue, body, args.files or [], args.reply_to))


if __name__ == "__main__":
    linear.run(main)
