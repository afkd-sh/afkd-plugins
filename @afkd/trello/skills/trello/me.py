#!/usr/bin/env python3
# me.py — `trello.me`: the member the credential acts as.
#
# It prints the member's handle as JSON: its `id` the username, its `key`, and its
# `full_name`. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    argparse.ArgumentParser(description="The member this run acts as.").parse_args()
    found = trello.request("read me", "GET", "/members/me")
    built = trello.member_handle(found)
    if built is None:
        raise trello.undecodable("read me", "authed member missing id or username")
    trello.emit(built)


if __name__ == "__main__":
    trello.run(main)
