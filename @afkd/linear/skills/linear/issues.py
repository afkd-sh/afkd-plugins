#!/usr/bin/env python3
# issues.py — `linear.issues`: the issues every filter given keeps.
#
# Each of the action's filters is a flag of its name, and a filter that takes a list takes its
# flag once per item: `--state Todo --state "In Progress"` keeps an issue in either, and
# `--labels a --labels b` one carrying both. A time is RFC 3339 (`2026-11-17T09:30:00Z`), a
# due date `YYYY-MM-DD`. It prints the issues' `linear.Issue` handles as a JSON array, each
# read whole, workspace-wide unless `--team` says, in the order `--order` gives (by team, then
# state, then top to bottom as Linear shows a state), at most `--limit` (50). A filter Linear
# cannot use exits 1 with the plugin's sentence. It reads LINEAR_API_URL and
# LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print the issues the filters keep.")
    lists = {
        "id": "an identifier (ENG-123) or a uuid",
        "team": "a team's key",
        "state": "a workflow state's name",
        "labels": "a label every issue kept carries",
        "without_labels": "a label no issue kept carries",
        "assignee": 'a display name or email, "me", or "none" for nobody',
        "delegate": 'a display name or email, "me", or "none" for nobody',
        "project": "a project's name",
        "milestone": "a milestone's name",
        "parent": "keep its sub-issues: an identifier or a uuid",
    }
    for name, what in lists.items():
        parser.add_argument(f"--{name}", action="append", metavar="TEXT", help=what)
    parser.add_argument("--state_type", action="append", choices=linear.STATE_TYPES)
    parser.add_argument("--priority", action="append", choices=linear.PRIORITIES)
    parser.add_argument("--cycle", action="append", type=int, metavar="N", help="its number")
    parser.add_argument(
        "--blocked",
        choices=("true", "false"),
        help="true keeps an issue an open issue blocks, false one no open issue blocks",
    )
    parser.add_argument("--text", help="in the title or the description, case ignored")
    for name, _, _ in linear.TIME_FILTERS:
        parser.add_argument(f"--{name}", type=linear.rfc3339, metavar="TIME")
    for name, _ in linear.DUE_FILTERS:
        parser.add_argument(f"--{name}", metavar="YYYY-MM-DD")
    parser.add_argument(
        "--include_archived", action="store_true", help="archived and trashed issues too"
    )
    parser.add_argument(
        "--order", action="append", choices=linear.SORTS, help="at most three keys, in order"
    )
    parser.add_argument("--limit", type=int, default=50, metavar="N", help="1 to 250 (50)")
    args = vars(parser.parse_args())
    if args["blocked"] is not None:
        args["blocked"] = args["blocked"] == "true"
    linear.emit(linear.issues({name: value for name, value in args.items() if value is not None}))


if __name__ == "__main__":
    linear.run(main)
