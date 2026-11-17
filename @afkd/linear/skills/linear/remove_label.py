#!/usr/bin/env python3
# remove_label.py — `linear.remove_label`: a label taken off an issue.
#
# The label is named as add_label.py names it. A label the issue does not carry is left as it
# is. It prints the issue's `linear.Issue` handle as Linear then holds it, as the plugin's
# `remove_label` gives it back. A name neither the team nor the workspace has exits 1 naming
# the labels there are. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a label off an issue.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    parser.add_argument("label", help="the label's name, or its id")
    args = parser.parse_args()
    linear.emit(linear.change_label(args.issue, args.label, False))


if __name__ == "__main__":
    linear.run(main)
