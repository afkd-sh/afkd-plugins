#!/usr/bin/env python3
# read_description.py — print the active Linear issue's description.
#
# The read half of the bundled `linear` skill's description pair (its write half is
# set_description.py): it prints the description of the issue this run works on verbatim, as
# Linear holds it now. An issue with none prints an empty line and succeeds. It reads
# LINEAR_API_URL, LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with
# --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print the issue's description.")
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    print(linear.description(linear.issue_ref(args)))


if __name__ == "__main__":
    linear.run(main)
