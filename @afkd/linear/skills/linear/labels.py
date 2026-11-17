#!/usr/bin/env python3
# labels.py — `linear.labels`: the workspace's labels, and a team's.
#
# It prints the workspace's labels and, with `--team`, that team's as a JSON array of
# `linear.Label` handles, a workspace label's `team` null, as the plugin's `labels` gives them
# back. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print the workspace's labels, and a team's.")
    parser.add_argument("--team", help="a team's key (ENG): its labels too")
    args = parser.parse_args()
    linear.emit(linear.labels(args.team))


if __name__ == "__main__":
    linear.run(main)
