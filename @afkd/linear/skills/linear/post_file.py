#!/usr/bin/env python3
# post_file.py — put a local file on the active Linear issue.
#
# The put half of the bundled `linear` skill's files: it uploads the file at its argument the
# way the plugin's `files=` does, through Linear's `fileUpload` and a PUT to the url it signs,
# which never carries the credential. It then posts a comment holding the file as the plugin
# embeds one, after --comment's text when given: an image shown `![name](url)`, any other file
# linked `[name](url)`. --name sets the name it is shown under (the file's own by default). A
# file that is missing, is no regular file or is over 10 MiB is refused before any request.
#
# It reads LINEAR_API_URL, LINEAR_AUTHORIZATION and LINEAR_ISSUE_ID, the issue overridable with
# --issue, and exits 1 naming any failure.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def read_file(path):
    """The bytes of the file at `path`, at most MAX_FILE_BYTES; exits 1 saying why not."""
    if not os.path.isfile(path):
        linear.die(f"no file at {path}" if not os.path.exists(path) else f"{path} is no file")
    try:
        with open(path, "rb") as handle:
            data = handle.read(linear.MAX_FILE_BYTES + 1)
    except OSError as e:
        linear.die(f"cannot read {path}: {e}")
    if len(data) > linear.MAX_FILE_BYTES:
        mib = linear.MAX_FILE_BYTES // (1024 * 1024)
        linear.die(f"{path} is over the {mib} MiB a file may hold")
    return data


def main():
    parser = argparse.ArgumentParser(description="Put a file on the issue.")
    parser.add_argument("path", help="the file to put up")
    parser.add_argument("--name", help="the name it is shown under (the file's own by default)")
    parser.add_argument("--comment", metavar="TEXT", help="text to post above it")
    linear.add_issue_arg(parser)
    args = parser.parse_args()
    name = args.name if args.name is not None else os.path.basename(args.path)
    if not name.strip():
        parser.error("the file needs a name")

    data = read_file(args.path)
    issue = linear.issue_ref(args)
    content_type = linear.content_type_of(name)
    asset = linear.upload(name, content_type, data)
    body = linear.embed(args.comment or "", [(name, content_type, asset)])
    linear.call("post comment", "CommentCreate", {"input": {"issueId": issue, "body": body}})
    print(f"post_file.py: put {name} on {linear.issue_name(args)} at {asset}")


if __name__ == "__main__":
    linear.run(main)
