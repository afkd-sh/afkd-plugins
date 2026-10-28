#!/usr/bin/env python3
# list_files.py — list the files on the active Linear issue.
#
# The list half of the bundled `linear` skill's files (fetch_file.py fetches one,
# post_file.py puts one up). A Linear file is a markdown link to an asset url Linear hosts, an
# image embedded `![name](url)` or a file linked `[name](url)`, in the description or a
# comment: a screenshot a human pasted lives there. It prints one `N  name  url` line per file,
# numbered in the order the plugin's `fetch_files` writes them, the description's first and
# then the thread's oldest first, each url once and afkd's own markers left out; a link to
# anywhere else is no file. An issue with none prints one `no files` line and succeeds. It
# reads LINEAR_API_URL, LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with
# --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="List the files the issue links.")
    linear.add_issue_arg(parser)
    args = parser.parse_args()

    files = linear.issue_files(linear.issue_ref(args))
    if not files:
        print(f"no files on issue {linear.issue_name(args)}")
        return
    for n, (name, url) in enumerate(files, 1):
        print(f"{n}  {name}  {url}")


if __name__ == "__main__":
    linear.run(main)
