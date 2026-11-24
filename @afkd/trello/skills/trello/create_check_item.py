#!/usr/bin/env python3
# create_check_item.py — `trello.create_check_item`: an item added to a checklist.
#
# It adds the item NAME to the checklist CHECKLIST (its id, from `checklists.py`), at the
# bottom of it unless --at top, and prints the item as its `trello.Check_Item` handle, read
# back: its key `trello:<card id>/<item id>`. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Add an item to a checklist.")
    parser.add_argument("checklist", help="the checklist's id")
    parser.add_argument("name", help="the item's name")
    trello.add_place_arg(parser, "the top or the bottom of the checklist (default bottom)")
    args = parser.parse_args()
    made = trello.request(
        "create check item",
        "POST",
        f"/checklists/{trello.seg(args.checklist)}/checkItems",
        [("pos", args.at or "bottom")],
        body={"name": args.name},
    )
    id_ = trello.str_of(made, "id")
    if not id_:
        raise trello.undecodable("create check item", "check item has no id")

    def read():
        owner = trello.request(
            "read checklist",
            "GET",
            f"/checklists/{trello.seg(args.checklist)}",
            [("fields", "idCard")],
        )
        card = trello.str_of(owner, "idCard")
        if not card:
            raise trello.undecodable("read checklist", "no card")
        return trello.read_check_item(card, id_)

    trello.emit(trello.read_back("check item", read))


if __name__ == "__main__":
    trello.run(main)
