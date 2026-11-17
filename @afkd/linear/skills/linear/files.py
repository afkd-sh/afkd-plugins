#!/usr/bin/env python3
# files.py — `linear.files`: the files an issue links.
#
# A file on a Linear issue is a markdown link to a url Linear hosts, in the description or a
# comment: an image embedded, any other file linked. It prints the urls of every one the
# issue links as a JSON array, the description's first, then the comments' oldest first, each
# once, a link in one of afkd's own markers left out, as the plugin's `files` gives them back.
# download_file.py fetches one. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Print the urls of the files an issue links.")
    parser.add_argument("issue", help="the issue's identifier (ENG-123) or its uuid")
    args = parser.parse_args()
    linear.emit(linear.files(args.issue))


if __name__ == "__main__":
    linear.run(main)
