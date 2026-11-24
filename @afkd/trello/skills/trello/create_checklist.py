#!/usr/bin/env python3
# create_checklist.py — `trello.create_checklist`: a checklist made on a card.
#
# It makes the checklist NAME on the card and, in the order given, each of --items on it, and
# prints the checklist as its `trello.Checklist` handle, read back. An item Trello refuses
# leaves the checklist as far as it got, and the sentence says how far. The card is --card,
# else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN. Ports src/writes.rs's
# `create_checklist`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Make a checklist on a card.")
    parser.add_argument("name", help="the checklist's name")
    parser.add_argument("--items", action="append", metavar="NAME", help="an item; repeatable")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    items = args.items or []
    card_id, short = trello.card_ids(trello.card_ref(args))
    made = trello.request(
        "create checklist",
        "POST",
        f"/cards/{trello.seg(card_id)}/checklists",
        body={"name": args.name},
    )
    id_ = trello.str_of(made, "id")
    if not id_:
        raise trello.undecodable("create checklist", "checklist has no id")
    for done, item in enumerate(items):
        try:
            trello.request(
                "create check item",
                "POST",
                f"/checklists/{trello.seg(id_)}/checkItems",
                [("pos", "bottom")],
                body={"name": item},
            )
        except trello.TrelloError as e:
            raise trello.TrelloError(
                f"checklist {trello.quoted(args.name)} ({id_}) was made on card {short} "
                f"with {done} of its {len(items)} items, then: {e}"
            ) from None

    def read():
        found = trello.request(
            "read checklist",
            "GET",
            f"/checklists/{trello.seg(id_)}",
            [("checkItems", "all"), ("fields", "name,pos,idCard")],
        )
        built = trello.checklist_handle(found, short)
        if built is None:
            raise trello.undecodable("read checklist", "no id")
        return built

    trello.emit(trello.read_back("checklist", read))


if __name__ == "__main__":
    trello.run(main)
