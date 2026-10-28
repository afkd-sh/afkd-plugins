#!/usr/bin/env python3
# list_states.py — list the run's team's workflow states.
#
# The navigation read of the bundled `linear` skill (with list_issues.py): a state is where a
# Trello board has a list. It prints one `id  type  name` line per state, in the order the
# team's workflow shows them; the type is Linear's category (`backlog`, `unstarted`,
# `started`, `completed`, `canceled`, `triage`). It reads LINEAR_API_URL, LINEAR_AUTHORIZATION
# and LINEAR_TEAM_ID, the team overridable with --team, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="List the team's workflow states.")
    linear.add_team_arg(parser)
    args = parser.parse_args()

    team = linear.team_ref(args)
    states = linear.states(team)
    if not states:
        print(f"no states on team {team}")
        return
    for state in states:
        print(f"{state.get('id')}  {state.get('type')}  {state.get('name')}")


if __name__ == "__main__":
    linear.run(main)
