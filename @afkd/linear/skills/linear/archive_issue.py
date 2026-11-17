#!/usr/bin/env python3
# archive_issue.py — `linear.archive_issue`: an issue archived.
#
# It archives the issue, its sub-issues with it, as Linear does, and prints its `linear.Issue`
# handle as Linear then holds it, `archived` true, as the plugin's `archive_issue` gives it
# back. An archived issue leaves every read but `issues.py --include_archived`. It reads
# LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Archive an issue.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    args = parser.parse_args()
    linear.emit(linear.archive_issue(args.issue))


if __name__ == "__main__":
    linear.run(main)
