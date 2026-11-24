#!/usr/bin/env python3
# remove_label.py — `trello.remove_label`: a label taken off a card.
#
# The label is named by its exact name or its id, on the card's own board. A label the card
# does not carry, or the board does not have, is already off the card: the script prints the
# card and sends no write. A name several labels share exits 1 listing their ids. It prints the
# card as its `trello.Card` handle, read back. The card is --card, else $TRELLO_CARD_ID. It
# reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Take a label off a card.")
    parser.add_argument("label", help="the label's name, or its id")
    trello.add_card_arg(parser)
    args = parser.parse_args()
    card = trello.card_ref(args)
    here = trello.request(
        "read card", "GET", f"/cards/{trello.seg(card)}", [("fields", "idBoard,idLabels")]
    )
    board = trello.str_of(here, "idBoard")
    if not board:
        raise trello.undecodable("read card", "card has no board")
    labels = trello.board_labels(board)
    named = [
        trello.str_of(label, "id")
        for label in labels
        if args.label in (trello.str_of(label, "id"), trello.str_of(label, "name"))
    ]
    carried = here.get("idLabels") if isinstance(here.get("idLabels"), list) else []
    if named:
        label = trello.named_or_id("label", args.label, labels)
        if label in carried:
            trello.request(
                "remove label", "DELETE", f"/cards/{trello.seg(card)}/idLabels/{trello.seg(label)}"
            )
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
