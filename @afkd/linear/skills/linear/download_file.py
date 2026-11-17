#!/usr/bin/env python3
# download_file.py — `linear.download_file`: one file Linear hosts, saved.
#
# It fetches the file at the url, from Linear and from no other host, with the credential
# Linear's file host asks for and without it on a redirect, and writes it to the absolute path
# `to` through a temp file renamed onto it. It prints `null`, what the plugin's
# `download_file` gives back. A relative `to`, a url Linear does not host and a file over
# 10 MiB exit 1. It reads LINEAR_API_URL and LINEAR_AUTHORIZATION.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import linear_api as linear  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Save a file Linear hosts.")
    parser.add_argument("url", help="the file's url, as files.py prints it")
    parser.add_argument("to", help="the absolute path it is written to")
    args = parser.parse_args()
    if not os.path.isabs(args.to):
        linear.die(f"download_file: to must be absolute, not {linear.quoted(args.to)}")
    data = linear.download(args.url)
    try:
        linear.write_atomic(args.to, data)
    except OSError as e:
        linear.die(f"download_file: cannot write {args.to}: {e}")
    linear.emit(None)


if __name__ == "__main__":
    linear.run(main)
