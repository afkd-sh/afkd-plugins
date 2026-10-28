#!/usr/bin/env python3
# post_comment.py — post a comment on the active Linear issue.
#
# The post half of the bundled `linear` skill (its read half is list_comments.py): it posts
# its one argument, verbatim, as a comment on the issue this run works on. It reads
# LINEAR_API_URL, LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with
# --issue. It refuses SKILL.md's placeholder and an empty comment (exit 2) before any request,
# and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR COMMENT>"


def main():
    parser = argparse.ArgumentParser(description="Post a comment on the issue.")
    parser.add_argument("text", help="the comment, verbatim")
    linear.add_issue_arg(parser)
    args = parser.parse_args()
    if not args.text.strip():
        parser.error("the comment is empty")
    linear.refuse_placeholder(args.text, PLACEHOLDER)

    issue = linear.issue_ref(args)
    linear.call(
        "post comment", "CommentCreate", {"input": {"issueId": issue, "body": args.text}}
    )
    print(f"post_comment.py: posted a comment on {linear.issue_name(args)}")


if __name__ == "__main__":
    linear.run(main)
