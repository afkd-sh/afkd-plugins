#!/usr/bin/env python3
# set_description.py — replace the active Linear issue's description.
#
# The write half of the bundled `linear` skill's description pair (its read half is
# read_description.py): it replaces the whole description of the issue this run works on with
# --desc, or with the text of the file --desc-file names, exactly one of the two (exit 2
# otherwise, before any request). It reads LINEAR_API_URL, LINEAR_AUTHORIZATION and
# LINEAR_ISSUE_ID, the issue overridable with --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Replace the issue's description.")
    text = parser.add_mutually_exclusive_group(required=True)
    text.add_argument("--desc", help="the new description")
    text.add_argument("--desc-file", metavar="PATH", help="a file holding the new description")
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    body = args.desc if args.desc is not None else linear.read_text_file(args.desc_file)
    issue = linear.issue_ref(args)
    linear.call("describe issue", "IssueUpdate", {"id": issue, "input": {"description": body}})
    print(f"set_description.py: replaced the description of {linear.issue_name(args)}")


if __name__ == "__main__":
    linear.run(main)
