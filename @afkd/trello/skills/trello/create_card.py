#!/usr/bin/env python3
# create_card.py — `trello.create_card`: a card filed in a list.
#
# It files a card NAME in the list LIST of the board, at the bottom of it unless --at top, and
# prints the card as its `trello.Card` handle, read back. The list, each label and each member
# (`me` included) are named as the board names them, matched exactly, and resolved BEFORE the
# card is made: a name the board lacks exits 1 naming what it has, one several share lists
# their ids, and nothing is created. The description is --description, or the text of
# --description_file, at most 16 384 characters, as is checked before any request, and the
# placeholder title is refused (exit 2). --due is an RFC 3339 time. The board is --board, else
# $TRELLO_BOARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN. Ports src/writes.rs's
# `create_card`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402

PLACEHOLDER = "<REPLACE WITH YOUR CARD TITLE>"


def main():
    parser = argparse.ArgumentParser(description="File a card in a list.")
    parser.add_argument("list", help="the list's name, or its id")
    parser.add_argument("name", help="the card's name")
    trello.add_board_arg(parser)
    trello.add_description_args(parser)
    trello.add_place_arg(parser, "the top or the bottom of the list (default bottom)")
    parser.add_argument("--labels", action="append", metavar="NAME", help="a label; repeatable")
    parser.add_argument(
        "--members", action="append", metavar="USERNAME", help='a member or "me"; repeatable'
    )
    parser.add_argument("--due", type=trello.rfc3339, metavar="RFC3339", help="when it is due")
    args = parser.parse_args()
    trello.refuse_placeholder(args.name, PLACEHOLDER)
    description = trello.description_of(args)

    board_id, _ = trello.read_board(trello.board_ref(args))
    lists = trello.open_lists(board_id)
    labels = trello.board_labels(board_id)
    members = trello.candidates(trello.board_members(board_id), "username")

    list_id = trello.named_or_id("list", args.list, lists)
    label_ids = []
    for name in args.labels or []:
        id_ = trello.named_or_id("label", name, labels)
        if id_ not in label_ids:
            label_ids.append(id_)
    member_ids = []
    for who in args.members or []:
        id_ = trello.one_named("member", trello.username_of(who), members)
        if id_ not in member_ids:
            member_ids.append(id_)

    body = {"idList": list_id, "name": args.name}
    if description is not None:
        body["desc"] = description
    body["pos"] = args.at or "bottom"
    if label_ids:
        body["idLabels"] = label_ids
    if member_ids:
        body["idMembers"] = member_ids
    if args.due is not None:
        body["due"] = trello.iso(args.due)
    made = trello.request("create card", "POST", "/cards", body=body)
    id_ = trello.str_of(made, "id")
    if not id_:
        raise trello.undecodable("create card", "card has no id")
    trello.emit(trello.read_back("card", lambda: trello.read_card(id_)))


if __name__ == "__main__":
    trello.run(main)
