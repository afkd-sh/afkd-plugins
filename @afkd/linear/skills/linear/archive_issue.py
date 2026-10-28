#!/usr/bin/env python3
# archive_issue.py — archive the active Linear issue, or with --unarchive restore one.
#
# The archive verb of the bundled `linear` skill: an issue that has resolved to nothing leaves
# every list and every claim. An archived issue appears in no read this skill makes, so
# restoring one takes its identifier or id, passed with --issue. It reads LINEAR_API_URL,
# LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with --issue, and exits 1
# naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Archive the issue, or restore it.")
    parser.add_argument("--unarchive", action="store_true", help="restore it instead")
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    issue = linear.issue_ref(args)
    if args.unarchive:
        linear.call("unarchive issue", "IssueUnarchive", {"id": issue})
        print(f"archive_issue.py: restored {linear.issue_name(args)}")
    else:
        linear.call("archive issue", "IssueArchive", {"id": issue})
        print(f"archive_issue.py: archived {linear.issue_name(args)}")


if __name__ == "__main__":
    linear.run(main)
