#!/usr/bin/env python3
# move_sub_issue.py — move one of the active Linear issue's sub-issues to a workflow state.
#
# The write half of the bundled `linear` skill's sub-issues (its read half is
# list_sub_issues.py): moving a sub-issue to a done state is how a part of the issue is ticked
# off, as the plugin's `move_sub_issue` does. The sub-issue is named by its identifier or id
# and must be one of this issue's (exit 1 otherwise, with nothing written); the state by id or
# exact name, read against the team's states. It reads LINEAR_API_URL, LINEAR_AUTHORIZATION,
# LINEAR_TEAM_ID and LINEAR_ISSUE_ID, overridable with --team and --issue, and exits 1 naming
# any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Move a sub-issue to a workflow state.")
    parser.add_argument("sub_issue", help="the sub-issue, by identifier (ENG-12) or id")
    parser.add_argument("state", help="the state, by id or exact name")
    linear.add_issue_arg(parser)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    issue, team = linear.issue_ref(args), linear.team_ref(args)
    children = linear.pages(
        "read sub-issues", "IssueChildren", {"id": issue}, ("issue", "children")
    )
    child = next(
        (c for c in children if args.sub_issue in (c.get("id"), c.get("identifier"))), None
    )
    if child is None:
        linear.die(f"{args.sub_issue} is no sub-issue of {linear.issue_name(args)}")
    state = linear.resolve("state", linear.states(team), args.state, team)
    linear.call(
        "move sub-issue", "IssueUpdate", {"id": child["id"], "input": {"stateId": state["id"]}}
    )
    print(f"move_sub_issue.py: moved {child.get('identifier')} to {state['name']}")


if __name__ == "__main__":
    linear.run(main)
