#!/usr/bin/env python3
# list_comments.py — read the active Linear issue's comments.
#
# The read half of the bundled `linear` skill (its post half is post_comment.py): it prints
# the newest --limit comments on the issue this run works on (default 50), oldest first, each
# a line naming its author and its time, ` (edited)` when it was, then its body verbatim. An
# issue with none prints one `no comments` line and succeeds. It reads LINEAR_API_URL,
# LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with --issue, and exits 1
# naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def render(comment):
    """One comment: `AUTHOR · CREATED_AT`, then its body."""
    edited = " (edited)" if comment.get("editedAt") else ""
    header = f"{linear.author(comment)} · {comment.get('createdAt')}{edited}"
    return f"{header}\n{comment.get('body') or ''}"


def main():
    parser = argparse.ArgumentParser(description="Print the issue's comments, oldest first.")
    parser.add_argument("--limit", type=linear.positive_int, default=50)
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    comments = linear.thread(linear.issue_ref(args))[-args.limit :]
    if not comments:
        print(f"no comments on issue {linear.issue_name(args)}")
        return
    print("\n\n".join(render(c) for c in comments))


if __name__ == "__main__":
    linear.run(main)
