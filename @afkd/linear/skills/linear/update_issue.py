#!/usr/bin/env python3
# update_issue.py — `linear.update_issue`: an issue's fields changed.
#
# Each field given is set and each one left out is left as it is: --title, --description (or
# --description_file; "" empties it), --files embedded after the description, --state,
# --team (a team's key, beside a --state of that team: the issue moves team), --priority,
# --estimate, --due, --assignee, --delegate, --project, --milestone, --cycle and --parent.
# --clear NAME clears one of estimate, due, assignee, delegate, project, milestone, cycle or
# parent, the action's `nil`. --at top or bottom, or --after ISSUE, places the issue in its
# state, the new one or the one it is in. It prints the issue's `linear.Issue` handle as
# Linear then holds it, as the plugin's `update_issue` gives it back. Every name is looked up,
# and every file read, before anything changes. It refuses SKILL.md's placeholder (exit 2)
# before any request. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR NEW TITLE>"


def main():
    parser = argparse.ArgumentParser(description="Change an issue's fields.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    parser.add_argument("--title", metavar="TEXT")
    linear.add_field_args(parser)
    parser.add_argument("--team", help="the team's key (ENG) it moves to, beside --state")
    parser.add_argument(
        "--clear", action="append", choices=linear.CLEARABLE, help="clear this field"
    )
    args = parser.parse_args()
    if args.title is not None:
        linear.refuse_placeholder(args.title, PLACEHOLDER)
    change = linear.given(args)
    for name in change.pop("clear", []):
        if name in change:
            parser.error(f"--{name} and --clear {name} cannot both be given")
        change[name] = None
    linear.emit(linear.update_issue(change))


if __name__ == "__main__":
    linear.run(main)
