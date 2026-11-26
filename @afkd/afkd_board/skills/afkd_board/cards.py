#!/usr/bin/env python3
# cards.py — `afkd_board.cards`: the cards every filter given keeps, in the board's order.
#
# It prints an array of card handles. The board's order is by each card's list's place, then its
# place in the list, a list's archived cards after its others, the last archived first. A filter
# left out keeps every card. --list names a list (repeat it for several: a card in any of them),
# --label a label the card must carry (repeat it: every one), --without_label one it must not;
# --done and --blocked are `true` or `false`; --text is looked for in the title and the
# description, any case; --include_archived keeps archived cards too; --limit is 1 to 250, 50 by
# default.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import afkd_board_api as api  # noqa: E402

DEFAULT_LIMIT = 50
MAX_LIMIT = 250


def merge(lists, cards, limit):
    """The cards of several queries in board order, cut to `limit`: by their list's place, then
    archived after not. The sort is stable, so each query's own order is kept."""
    names = [l["name"] for l in lists]

    def place(card):
        at = names.index(card["list"]) if card["list"] in names else len(names)
        return (at, card["archived"])

    return sorted(cards, key=place)[:limit]


def main():
    parser = argparse.ArgumentParser(description="Read the cards every filter keeps.")
    parser.add_argument("--list", action="append", dest="lists", metavar="NAME")
    parser.add_argument("--label", action="append", default=[], metavar="NAME")
    parser.add_argument("--without_label", action="append", default=[], metavar="NAME")
    parser.add_argument("--done", type=api.truth, metavar="true|false")
    parser.add_argument("--blocked", type=api.truth, metavar="true|false")
    parser.add_argument("--text", metavar="TEXT")
    parser.add_argument("--include_archived", action="store_true")
    parser.add_argument("--limit", type=int, default=DEFAULT_LIMIT)
    args = parser.parse_args()
    if not 1 <= args.limit <= MAX_LIMIT:
        api.die(f"cards: limit is 1 to {MAX_LIMIT}, not {args.limit}")
    board = api.board()
    lists = [None]
    if args.lists is not None:
        lists = list(dict.fromkeys(args.lists))
        for name in lists:
            api.list_id(board, name)
    cards = []
    for list_ in lists:
        for archived in [False, True] if args.include_archived else [False]:
            query = []
            if list_ is not None:
                query.append(("list", list_))
            query += [("labels", l) for l in args.label]
            query += [("without_labels", l) for l in args.without_label]
            if args.text is not None:
                query.append(("text", args.text))
            query.append(("archived", str(archived).lower()))
            if args.done is not None:
                query.append(("done", str(args.done).lower()))
            if args.blocked is not None:
                query.append(("blocked", str(args.blocked).lower()))
            query.append(("limit", str(args.limit)))
            cards += api.request("GET", "/cards", query=query)
    kept = merge(board["lists"], cards, args.limit)
    api.emit([api.card_handle(board["id"], c) for c in kept])


if __name__ == "__main__":
    api.run(main)
