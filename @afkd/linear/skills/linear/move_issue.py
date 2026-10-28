#!/usr/bin/env python3
# move_issue.py — move the active Linear issue to a workflow state, and place it there.
#
# The board-move verb of the bundled `linear` skill (its read half is list_states.py): it puts
# the issue this run works on into the state its argument names, by id or by exact name, read
# against the team's states. It lands at the bottom of the state's manual order by default,
# where the plugin puts back an issue it gave up; --pos top puts it first, where the plugin
# claims from, and --above or --below ISSUE right beside an issue already there, half-way to
# its neighbour. The issue itself is left out of the neighbours, so a move within its own
# state lands right.
#
# Nothing is written when the state or the anchor is not found (exit 1), when the anchor is
# the issue moving (exit 2), or when two placements are given (exit 2). It reads
# LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_TEAM_ID and LINEAR_ISSUE_ID, overridable with
# --team and --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Move the issue to a workflow state.")
    parser.add_argument("state", help="the state, by id or exact name")
    linear.add_placement_args(parser, "bottom")
    linear.add_issue_arg(parser)
    linear.add_team_arg(parser)
    args = parser.parse_args()

    issue, team = linear.issue_ref(args), linear.team_ref(args)
    state = linear.resolve("state", linear.states(team), args.state, team)
    order = linear.sort_order_for(
        team, state, args.pos, args.above, args.below, linear.moving_refs(args)
    )
    linear.call(
        "move issue",
        "IssueUpdate",
        {"id": issue, "input": {"stateId": state["id"], "sortOrder": order}},
    )
    print(
        f"move_issue.py: moved {linear.issue_name(args)} to {state['name']} "
        f"({linear.placed(args)})"
    )


if __name__ == "__main__":
    linear.run(main)
