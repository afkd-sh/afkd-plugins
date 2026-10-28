#!/usr/bin/env python3
# set_title.py — retitle the active Linear issue.
#
# The rename verb of the bundled `linear` skill: it replaces the whole title of the issue this
# run works on with its one argument. It reads LINEAR_API_URL, LINEAR_AUTHORIZATION and
# LINEAR_ISSUE_ID, the issue overridable with --issue. It refuses SKILL.md's placeholder and
# an empty title (exit 2) before any request, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR NEW TITLE>"


def main():
    parser = argparse.ArgumentParser(description="Retitle the issue.")
    parser.add_argument("title", help="the new title, whole")
    linear.add_issue_arg(parser)
    args = parser.parse_args()
    if not args.title.strip():
        parser.error("the title is empty")
    linear.refuse_placeholder(args.title, PLACEHOLDER)

    issue = linear.issue_ref(args)
    linear.call("rename issue", "IssueUpdate", {"id": issue, "input": {"title": args.title}})
    print(f"set_title.py: retitled {linear.issue_name(args)}")


if __name__ == "__main__":
    linear.run(main)
