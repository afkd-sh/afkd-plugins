#!/usr/bin/env python3
# add_relation.py — `linear.add_relation`: an issue related to another.
#
# Afterwards the other issue's identifier is in the issue's field named for the relation, and
# the issue's in the other's matching one: `blocked_by` makes the other a blocker of the
# issue, which afkd then claims only once the blocker is done. A relation the issue already
# holds is left as it is. It prints the issue's `linear.Issue` handle as Linear then holds it,
# as the plugin's `add_relation` gives it back. It reads LINEAR_API_URL and
# LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Relate an issue to another.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    parser.add_argument("relation", choices=linear.RELATIONS, help="the issue's field")
    parser.add_argument("other", help="the other issue's identifier or uuid")
    args = parser.parse_args()
    linear.emit(linear.add_relation(args.issue, args.relation, args.other))


if __name__ == "__main__":
    linear.run(main)
