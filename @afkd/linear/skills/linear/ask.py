#!/usr/bin/env python3
# ask.py — ask a human on the active Linear issue, and park the run.
#
# The "I cannot build this as it stands" action of the bundled `linear` skill. In one step it
# posts its one argument as a comment on the issue this run works on and then writes the park
# marker `park` into the run's scratch dir ($AFKD_SCRATCH_DIR). The plugin reads that marker
# when the run ends and parks the issue instead of failing it: the `Needs Info` label goes
# on, the claim is released, and nothing else moves. A later poll finds the issue once a human
# replies, and a fresh run starts with the answer in its brief. The marker is written only
# after the comment lands, so a failed post never parks an issue with no question on it, and
# the scratch dir is checked before the post, so a question is never posted that nothing
# parks. The marker's name is the plugin's PARK_FILE (src/main.rs).
#
# It reads LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_ISSUE_ID and AFKD_SCRATCH_DIR, the
# issue overridable with --issue. It refuses SKILL.md's placeholder and an empty question
# (exit 2), and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR QUESTIONS>"

# The park marker, under $AFKD_SCRATCH_DIR: src/main.rs's PARK_FILE.
PARK_FILE = "park"


def main():
    parser = argparse.ArgumentParser(description="Ask on the issue and park the run.")
    parser.add_argument("message", help="the question, or why the issue cannot be built")
    linear.add_issue_arg(parser)
    args = parser.parse_args()
    if not args.message.strip():
        parser.error("the question is empty")
    linear.refuse_placeholder(args.message, PLACEHOLDER)

    _, _, scratch = linear.need_env(
        linear.ENV_API_URL, linear.ENV_AUTHORIZATION, "AFKD_SCRATCH_DIR"
    )
    issue = linear.issue_ref(args)
    linear.call(
        "post question", "CommentCreate", {"input": {"issueId": issue, "body": args.message}}
    )

    # Never cwd-relative: a worktree the run works in is torn down before the plugin reads it.
    marker = os.path.join(scratch, PARK_FILE)
    try:
        with open(marker, "w", encoding="utf-8"):
            pass
    except OSError as e:
        linear.die(f"posted the question but could not write the park marker {marker}: {e}")
    print(f"ask.py: asked on {linear.issue_name(args)} and parked the run awaiting a reply")


if __name__ == "__main__":
    linear.run(main)
