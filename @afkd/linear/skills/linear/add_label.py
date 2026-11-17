#!/usr/bin/env python3
# add_label.py — `linear.add_label`: a label put on an issue.
#
# The label is named as Linear shows it, or by its id: the issue's team's label of that name,
# else the workspace's. A label the issue already carries is left as it is. It prints the
# issue's `linear.Issue` handle as Linear then holds it, as the plugin's `add_label` gives it
# back. A name neither the team nor the workspace has exits 1 naming the labels there are, and
# one two labels carry exits 1 listing their ids. It reads LINEAR_API_URL and
# LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a label on an issue.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    parser.add_argument("label", help="the label's name, or its id")
    args = parser.parse_args()
    linear.emit(linear.change_label(args.issue, args.label, True))


if __name__ == "__main__":
    linear.run(main)
