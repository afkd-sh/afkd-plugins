#!/usr/bin/env python3
# issue.py — `linear.issue`: one issue, by its identifier or its uuid.
#
# It reads the issue whole, every list in it to its end, and prints its `linear.Issue` handle
# as JSON, as the plugin's `issue` gives it back. By its uuid an issue is read anew even after
# it moved team and took another identifier. An issue Linear does not hold exits 1 naming it.
# It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print one issue's handle.")
    parser.add_argument("id", help="the issue's identifier (ENG-123) or its uuid")
    args = parser.parse_args()
    linear.emit(linear.read_issue(args.id))


if __name__ == "__main__":
    linear.run(main)
