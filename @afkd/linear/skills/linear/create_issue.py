#!/usr/bin/env python3
# create_issue.py — `linear.create_issue`: an issue filed.
#
# It files an issue titled `title` in the team whose key is `team`, in the team's default
# state unless --state names one, each field it is given set: --description (or
# --description_file), --files embedded after it, --priority, --estimate, --due, --labels (once
# per label), --assignee, --delegate, --project, --milestone, --cycle and --parent. --at top or
# bottom, or --after ISSUE, places it in its state. A team, a state, a label, a user, a project
# or a milestone is named as Linear shows it, or by its id. It prints the new issue's
# `linear.Issue` handle as JSON, as the plugin's `create_issue` gives it back. Every name is
# looked up, and every file read, before anything is filed: a name that matches nothing exits 1
# naming what there is, and one that matches several listing their ids. It refuses SKILL.md's
# placeholder and an empty title (exit 2) before any request. It reads LINEAR_API_URL and
# LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR ISSUE TITLE>"


def main():
    parser = argparse.ArgumentParser(description="File an issue.")
    parser.add_argument("team", help="the team's key (ENG)")
    parser.add_argument("title", help="the new issue's title")
    linear.add_field_args(parser)
    parser.add_argument("--labels", action="append", metavar="LABEL", help="a label, by name")
    args = parser.parse_args()
    if not args.title.strip():
        parser.error("the title is empty")
    linear.refuse_placeholder(args.title, PLACEHOLDER)
    linear.emit(linear.create_issue(linear.given(args)))


if __name__ == "__main__":
    linear.run(main)
