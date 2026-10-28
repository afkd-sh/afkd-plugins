#!/usr/bin/env python3
# list_issues.py — list the open issues in one of the run's team's workflow states.
#
# The navigation read of the bundled `linear` skill (with list_states.py): it prints one
# `identifier  title` line per issue in the state its argument names, by id or exact name, in
# the state's manual order, the order the plugin claims in, at most --limit of them (default
# 50). A state with none prints one `no issues` line and succeeds. It reads LINEAR_API_URL,
# LINEAR_AUTHORIZATION and LINEAR_TEAM_ID, the team overridable with --team, and exits 1
# naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="List the issues in a workflow state.")
    parser.add_argument("state", help="the state, by id or exact name")
    parser.add_argument("--limit", type=linear.positive_int, default=50)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    team = linear.team_ref(args)
    state = linear.resolve("state", linear.states(team), args.state, team)
    issues = linear.state_issues(team, state["id"], args.limit)
    if not issues:
        print(f"no issues in state {state['name']}")
        return
    for issue in issues:
        print(f"{issue.get('identifier')}  {issue.get('title')}")


if __name__ == "__main__":
    linear.run(main)
