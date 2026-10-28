#!/usr/bin/env python3
# list_sub_issues.py — list the active Linear issue's sub-issues.
#
# The read half of the bundled `linear` skill's sub-issues (its write half is
# move_sub_issue.py): a sub-issue is how an issue carries its parts, where a Trello card
# carries a checklist. It prints one `- ENG-12 (In Progress) title` line per sub-issue, in
# Linear's order, the line the plugin's brief and `list_sub_issues` write. An issue with none
# prints one `no sub-issues` line and succeeds. It reads LINEAR_API_URL, LINEAR_AUTHORIZATION
# and LINEAR_ISSUE_ID, the issue overridable with --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="List the issue's sub-issues.")
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    children = list(
        linear.pages(
            "read sub-issues",
            "IssueChildren",
            {"id": linear.issue_ref(args)},
            ("issue", "children"),
        )
    )
    if not children:
        print(f"no sub-issues on issue {linear.issue_name(args)}")
        return
    for child in children:
        state = (child.get("state") or {}).get("name") or ""
        print(f"- {child.get('identifier')} ({state}) {child.get('title')}")


if __name__ == "__main__":
    linear.run(main)
