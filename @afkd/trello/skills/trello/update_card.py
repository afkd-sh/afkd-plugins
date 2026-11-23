#!/usr/bin/env python3
# update_card.py — `trello.update_card`: what a card says, where it sits and when it is due.
#
# It changes the card as the flags say, each one left out leaving its field as it is, in ONE
# `PUT` with a JSON body, and prints the card as its `trello.Card` handle, read back. With no
# flag it sends nothing. --name and --description (or --description_file's text, at most 16 384
# characters, checked before any request) REPLACE the field whole; the placeholder title is
# refused (exit 2). --list is a list's name, or its id, resolved on the card's own board, and a
# card moved to another list lands at the top unless --at bottom says otherwise, as
# a service's `move_to` puts it; --at alone places the card in the list it is in. --due is an
# RFC 3339 time, `--clear due` takes the date away, and --due_complete marks it done or not.
# The card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN. Ports
# src/writes.rs's `update_card`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR NEW TITLE>"


def main():
    parser = argparse.ArgumentParser(description="Change a card.")
    trello.add_card_arg(parser)
    parser.add_argument("--name", help="the card's new name")
    trello.add_description_args(parser)
    parser.add_argument("--list", help="move it to this list, by name or id, of its board")
    trello.add_place_arg(parser, "the top or the bottom of its list (a moved card: the top)")
    due = parser.add_mutually_exclusive_group()
    due.add_argument("--due", type=trello.rfc3339, metavar="RFC3339", help="when it is due")
    due.add_argument("--clear", choices=("due",), help="take the due date away")
    parser.add_argument(
        "--due_complete", choices=("true", "false"), help="whether its due date is done"
    )
    args = parser.parse_args()
    if args.name is not None:
        trello.refuse_placeholder(args.name, PLACEHOLDER)
    description = trello.description_of(args)
    card = trello.card_ref(args)

    body = {}
    if args.name is not None:
        body["name"] = args.name
    if description is not None:
        body["desc"] = description
    if args.due is not None:
        body["due"] = trello.iso(args.due)
    elif args.clear == "due":
        body["due"] = None
    if args.due_complete is not None:
        body["dueComplete"] = args.due_complete == "true"
    if args.list is not None or args.at is not None:
        here = trello.request(
            "read card", "GET", f"/cards/{trello.seg(card)}", [("fields", "idBoard,idList")]
        )
        current = trello.str_of(here, "idList")
        target = current
        if args.list is not None:
            board = trello.str_of(here, "idBoard")
            if not board:
                raise trello.undecodable("read card", "card has no board")
            target = trello.named_or_id("list", args.list, trello.open_lists(board))
        # A card moved to another list lands at the top; one `--at` places in its own list
        # goes where it says.
        if target != current or args.at is not None:
            body["idList"] = target
            body["pos"] = args.at or "top"
    if body:
        trello.request("update card", "PUT", f"/cards/{trello.seg(card)}", body=body)
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
