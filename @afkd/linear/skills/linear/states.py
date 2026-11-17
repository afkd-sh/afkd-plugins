#!/usr/bin/env python3
# states.py — `linear.states`: a team's workflow states.
#
# It prints the team's workflow states as a JSON array of `linear.State` handles, in the order
# its workflow shows them, as the plugin's `states` gives them back. It reads LINEAR_API_URL
# and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print a team's workflow states.")
    parser.add_argument("team", help="the team's key (ENG)")
    args = parser.parse_args()
    linear.emit(linear.states(args.team))


if __name__ == "__main__":
    linear.run(main)
