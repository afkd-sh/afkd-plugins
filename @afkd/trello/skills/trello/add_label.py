#!/usr/bin/env python3
# add_label.py — `trello.add_label`: a board's label put on a card.
#
# The label is named by its exact name or its id, on the card's own board. It NEVER creates a
# label: a name the board lacks exits 1 naming the labels the board has, and one several labels
# share exits 1 listing their ids. A label the card already carries is left as it is, with no
# request sent. It prints the card as its `trello.Card` handle, read back. The card is --card,
# else $TRELLO_CARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description="Put a label on a card.")
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
    label = trello.named_or_id("label", args.label, trello.board_labels(board))
    carried = here.get("idLabels") if isinstance(here.get("idLabels"), list) else []
    if label not in carried:
        trello.request(
            "add label", "POST", f"/cards/{trello.seg(card)}/idLabels", [("value", label)]
        )
    trello.emit(trello.read_back("card", lambda: trello.read_card(card)))


if __name__ == "__main__":
    trello.run(main)
