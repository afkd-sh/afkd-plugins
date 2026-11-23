#!/usr/bin/env python3
# delete_checklist.py — `trello.delete_checklist`: a checklist deleted, its items with it.
#
# It deletes the checklist CHECKLIST (its id, from `checklists.py`) and prints `null`. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Delete a checklist.")
    parser.add_argument("checklist", help="the checklist's id")
    args = parser.parse_args()
    trello.request("delete checklist", "DELETE", f"/checklists/{trello.seg(args.checklist)}")
    trello.emit(None)


if __name__ == "__main__":
    trello.run(main)
