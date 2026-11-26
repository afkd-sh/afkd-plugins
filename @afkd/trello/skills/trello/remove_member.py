#!/usr/bin/env python3
# remove_member.py — `trello.remove_member`: a member taken off a card.
#
# MEMBER is a username, matched exactly, and defaults to `me`, the member this run acts as. A
# member who is not on the card, or whom the card's board does not have, is already off it:
# the script prints the card and sends no write. It prints the card as its `trello.Card`
# handle, read back. The card is --card, else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and
# TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a member off a card.")
    parser.add_argument(
        "member", nargs="?", default=trello.ME, help='a username, or "me" (the default)'
    )
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    here = trello.request(
        "read card", "GET", f"/cards/{trello.seg(card)}", [("fields", "idBoard,idMembers")]
    )
    on = here.get("idMembers") if isinstance(here.get("idMembers"), list) else []
    if args.member == trello.ME:
        member = trello.me()[0]
    else:
        board = trello.str_of(here, "idBoard")
        found = [
            trello.str_of(m, "id")
            for m in trello.board_members(board)
            if trello.str_of(m, "username") == args.member
        ]
        member = found[0] if found else None
    if member is not None and member in on:
        trello.request(
            "remove member", "DELETE", f"/cards/{trello.seg(card)}/idMembers/{trello.seg(member)}"
        )
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
