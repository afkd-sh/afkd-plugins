#!/usr/bin/env python3
# create_issue.py — file a new Linear issue in the run's team.
#
# The "file it, do not derail onto it" verb of the bundled `linear` skill: it files an issue
# titled by its argument, with --desc or --desc-file as its description, in the team this run
# works in. With --state it starts in that state, by id or exact name, at the bottom of its
# manual order, or where --pos, --above or --below ISSUE put it; with no --state Linear files
# it in the team's default state, and a placement is refused (exit 2). --parent ISSUE makes it
# a sub-issue of that issue. It prints the new issue's identifier, title and url.
#
# It reads LINEAR_API_URL, LINEAR_AUTHORIZATION and LINEAR_TEAM_ID, the team overridable with
# --team. It refuses SKILL.md's placeholder and an empty title (exit 2) before any request,
# files nothing when the state or the anchor is not found (exit 1), and exits 1 naming any
# failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR ISSUE TITLE>"


def main():
    parser = argparse.ArgumentParser(description="File a new issue in the team.")
    parser.add_argument("title", help="the new issue's title")
    text = parser.add_mutually_exclusive_group()
    text.add_argument("--desc", help="its description")
    text.add_argument("--desc-file", metavar="PATH", help="a file holding its description")
    parser.add_argument("--state", help="the state it starts in, by id or exact name")
    linear.add_placement_args(parser, None)
    parser.add_argument("--parent", metavar="ISSUE", help="file it as a sub-issue of ISSUE")
    linear.add_team_arg(parser)
    args = parser.parse_args()
    if not args.title.strip():
        parser.error("the title is empty")
    if args.state is None and (args.pos or args.above or args.below):
        parser.error("--pos, --above and --below place it in a --state")
    linear.refuse_placeholder(args.title, PLACEHOLDER)

    body = args.desc if args.desc is not None else ""
    if args.desc_file is not None:
        body = linear.read_text_file(args.desc_file)
    team = linear.team_ref(args)
    issue = {"teamId": team, "title": args.title}
    if body:
        issue["description"] = body
    if args.parent:
        issue["parentId"] = args.parent
    if args.state is not None:
        state = linear.resolve("state", linear.states(team), args.state, team)
        issue["stateId"] = state["id"]
        issue["sortOrder"] = linear.sort_order_for(
            team, state, args.pos or "bottom", args.above, args.below
        )
    data = linear.call("create issue", "NewIssue", {"input": issue})
    filed = linear.dig("create issue", data, ("issueCreate", "issue"))
    print(f'create_issue.py: created {filed.get("identifier")} "{args.title}" {filed.get("url")}')


if __name__ == "__main__":
    linear.run(main)
