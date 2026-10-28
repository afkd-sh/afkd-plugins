#!/usr/bin/env python3
# remove_label.py — take a label off the active Linear issue.
#
# A write half of the bundled `linear` skill's labels (its read half is list_labels.py): it takes
# the label its argument names off the issue, by id or by exact name, read against the team's and
# the workspace's labels. A name no label carries, or one two labels carry, exits 1 and writes
# nothing; the ids it prints tell them apart. A label the issue does not carry is a no-op success
# with no write. It reads LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_TEAM_ID and
# LINEAR_ISSUE_ID, overridable with --team and --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a label off the issue.")
    parser.add_argument("label", help="the label, by id or exact name")
    linear.add_issue_arg(parser)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    issue, team = linear.issue_ref(args), linear.team_ref(args)
    label = linear.resolve("label", linear.labels(team), args.label, team)
    name = linear.issue_name(args)
    if label["id"] not in linear.issue_label_ids(issue):
        print(f"remove_label.py: {name} does not carry {label['name']}; nothing to do")
        return
    linear.call("remove label", "IssueRemoveLabel", {"id": issue, "labelId": label["id"]})
    print(f"remove_label.py: took {label['name']} off {name}")


if __name__ == "__main__":
    linear.run(main)
