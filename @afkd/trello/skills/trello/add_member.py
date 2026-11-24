#!/usr/bin/env python3
# add_member.py — `trello.add_member`: a board member put on a card.
#
# MEMBER is a username on the card's own board, matched exactly, and defaults to `me`, the
# member this run acts as. A username the board lacks exits 1 naming the board's members. A
# member already on the card is left as is, with no request sent. It prints the card as its
# `trello.Card` handle, read back. The card is --card, else $TRELLO_CARD_ID. It reads
# TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a member on a card.")
    parser.add_argument(
        "member", nargs="?", default=trello.ME, help='a username, or "me" (the default)'
    )
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    here = trello.request(
        "read card", "GET", f"/cards/{trello.seg(card)}", [("fields", "idBoard,idMembers")]
    )
    board = trello.str_of(here, "idBoard")
    if not board:
        raise trello.undecodable("read card", "card has no board")
    members = trello.candidates(trello.board_members(board), "username")
    member = trello.one_named("member", trello.username_of(args.member), members)
    on = here.get("idMembers") if isinstance(here.get("idMembers"), list) else []
    if member not in on:
        trello.request(
            "add member", "POST", f"/cards/{trello.seg(card)}/idMembers", [("value", member)]
        )
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
