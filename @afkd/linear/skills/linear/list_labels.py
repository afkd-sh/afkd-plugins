#!/usr/bin/env python3
# list_labels.py — list the labels an issue of the run's team can carry.
#
# The read half of the bundled `linear` skill's labels (its write half is add_label.py and
# remove_label.py): it prints one `id  color  name` line per label, the team's and the
# workspace's, a workspace label marked ` [workspace]` and one the active issue carries
# ` [on issue]`. A team with none prints one `no labels` line and succeeds. It reads
# LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_TEAM_ID and LINEAR_ISSUE_ID, overridable with
# --team and --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="List the team's and the workspace's labels.")
    linear.add_issue_arg(parser)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    team = linear.team_ref(args)
    labels = linear.labels(team)
    if not labels:
        print(f"no labels on team {team}")
        return
    issue = args.issue or os.environ.get(linear.ENV_ISSUE_ID)
    on_issue = linear.issue_label_ids(issue) if issue else set()
    for label in labels:
        marks = "" if label.get("team") else " [workspace]"
        marks += " [on issue]" if label.get("id") in on_issue else ""
        print(f"{label.get('id')}  {label.get('color')}  {label.get('name')}{marks}")


if __name__ == "__main__":
    linear.run(main)
