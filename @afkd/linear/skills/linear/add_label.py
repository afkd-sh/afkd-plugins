#!/usr/bin/env python3
# add_label.py — put a label on the active Linear issue.
#
# A write half of the bundled `linear` skill's labels (its read half is list_labels.py): it puts
# the label its argument names on the issue, by id or by exact name, read against the team's and
# the workspace's labels. A name no label carries, or one two labels carry, exits 1 and writes
# nothing; the ids it prints tell them apart. A label already on the issue is a no-op success with
# no write. It reads LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_TEAM_ID and LINEAR_ISSUE_ID,
# overridable with --team and --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a label on the issue.")
    parser.add_argument("label", help="the label, by id or exact name")
    linear.add_issue_arg(parser)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    issue, team = linear.issue_ref(args), linear.team_ref(args)
    label = linear.resolve("label", linear.labels(team), args.label, team)
    name = linear.issue_name(args)
    if label["id"] in linear.issue_label_ids(issue):
        print(f"add_label.py: {name} already carries {label['name']}; nothing to do")
        return
    linear.call("add label", "IssueAddLabel", {"id": issue, "labelId": label["id"]})
    print(f"add_label.py: put {label['name']} on {name}")


if __name__ == "__main__":
    linear.run(main)
