#!/usr/bin/env python3
# fetch_file.py — download one file the active Linear issue links.
#
# The fetch half of the bundled `linear` skill's files (list_files.py lists them): it names
# the file by its number in list_files.py's listing or its exact url, or with --name by its
# name, the newest file of that name, the last in the listing. It writes the file to --to, or
# into $AFKD_SCRATCH_DIR/attachments/ under the name the plugin's `fetch_files` writes it
# under, through a temp file renamed into place, and prints the path written. With --name and
# no file of that name it says so, writes nothing and succeeds. The download carries the
# credential and drops it on a redirect, and a file over 10 MiB is refused.
#
# It reads LINEAR_API_URL, LINEAR_AUTHORIZATION, LINEAR_ISSUE_ID and, without --to,
# AFKD_SCRATCH_DIR, the issue overridable with --issue. Both a file and --name, or neither,
# exit 2, and any failure exits 1 naming it.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Download a file the issue links.")
    parser.add_argument("file", nargs="?", help="its number in list_files.py, or its url")
    parser.add_argument("--name", help="the newest file named exactly this instead")
    parser.add_argument("--to", metavar="PATH", help="write here instead of the scratch dir")
    linear.add_issue_arg(parser)
    args = parser.parse_args()
    if (args.file is None) == (args.name is None):
        parser.error("name the file by its number or url, or by --name, not both")

    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if not args.to and not scratch:
        linear.die("AFKD_SCRATCH_DIR must be set (the download destination), or pass --to")
    issue = linear.issue_ref(args)
    files = linear.issue_files(issue)
    if args.name is not None:
        named = [n for n, (name, _) in enumerate(files) if name == args.name]
        if not named:
            print(f"fetch_file.py: no file named {args.name} on issue {linear.issue_name(args)}")
            return
        at = named[-1]
    elif args.file.isdigit() and 1 <= int(args.file) <= len(files):
        at = int(args.file) - 1
    else:
        at = next((n for n, (_, url) in enumerate(files) if url == args.file), None)
        if at is None:
            linear.die(f"no file {args.file} on issue {linear.issue_name(args)}")
    name, url = files[at]

    data = linear.download(url)
    if args.to:
        dest = args.to
    else:
        directory = os.path.join(scratch, "attachments")
        os.makedirs(directory, exist_ok=True)
        dest = os.path.join(
            directory, linear.file_name_for(at + 1, len(str(len(files))), name, url)
        )
    try:
        linear.write_atomic(dest, data)
    except OSError as e:
        linear.die(f"cannot write {dest}: {e}")
    print(dest)


if __name__ == "__main__":
    linear.run(main)
