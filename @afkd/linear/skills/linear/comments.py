#!/usr/bin/env python3
# comments.py — `linear.comments`: an issue's comments.
#
# It prints the issue's comments as a JSON array of `linear.Comment` handles, oldest first,
# each reply right after the comment it answers, afkd's own markers among them, as the
# plugin's `comments` gives them back. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print an issue's comments.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    args = parser.parse_args()
    linear.emit(linear.comments(args.issue))


if __name__ == "__main__":
    linear.run(main)
