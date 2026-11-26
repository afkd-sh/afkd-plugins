#!/usr/bin/env python3
# cards.py — `trello.cards`: a board's cards, filtered.
#
# It prints the cards every filter given keeps, each as its `trello.Card` handle, an array:
# list by list in the board's order, each list top to bottom, cut at --limit. A name a filter
# holds is checked against the board before any card is read, so a name the board lacks exits 1
# naming what it has. Archived cards, and cards in archived lists, are left out unless
# --include_archived. --list keeps the cards of any of the lists named, --labels the cards
# carrying every label named, --without_labels the cards carrying none of them, --members the
# cards any of the members named ("me" included) is on. The board is --board, else
# $TRELLO_BOARD_ID. It reads TRELLO_API_KEY and TRELLO_TOKEN. Ports src/reads.rs's `cards`.

import argparse
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "lib"))

import trello_api as trello  # noqa: E402


def limit_arg(text):
    """An argparse type: a count from 1 to 1000."""
    try:
        limit = int(text)
    except ValueError:
        raise argparse.ArgumentTypeError(f"{text!r} is no whole number") from None
    if not 1 <= limit <= trello.MAX_LIMIT:
        raise argparse.ArgumentTypeError(f"limit {limit} is outside 1 to {trello.MAX_LIMIT}")
    return limit


def board_cards(board_id, include_archived):
    """Every card of the board, open ones or, with `include_archived`, archived ones too,
    paged with Trello's `before` past its 1000: each page the cards below the lowest id the
    last one held."""
    path = f"/boards/{trello.seg(board_id)}/cards" + ("/all" if include_archived else "")
    seen, cards, before = set(), [], None
    while True:
        pairs = [("limit", str(trello.MAX_LIMIT)), ("fields", trello.CARD_FIELDS)]
        if before is not None:
            pairs.append(("before", before))
        page = trello.get_array("read cards", path, pairs)
        full = len(page) >= trello.MAX_LIMIT
        ids = [c["id"] for c in page if isinstance(c, dict) and isinstance(c.get("id"), str)]
        fresh = 0
        for card in page:
            id_ = card.get("id") if isinstance(card, dict) else None
            if isinstance(id_, str) and id_ not in seen:
                seen.add(id_)
                cards.append(card)
                fresh += 1
        if not (ids and full and fresh > 0):
            return cards
        before = min(ids)


def main():
    parser = argparse.ArgumentParser(description="A board's cards, filtered.")
    trello.add_board_arg(parser)
    parser.add_argument("--list", action="append", metavar="NAME", help="in this list; repeatable")
    parser.add_argument(
        "--labels", action="append", metavar="NAME", help="carrying this label; repeatable"
    )
    parser.add_argument(
        "--without_labels",
        action="append",
        metavar="NAME",
        help="carrying none of these labels; repeatable",
    )
    parser.add_argument(
        "--members",
        action="append",
        metavar="USERNAME",
        help='on this member, a username or "me"; repeatable',
    )
    parser.add_argument(
        "--include_archived", action="store_true", help="archived cards and lists too"
    )
    parser.add_argument("--limit", type=limit_arg, default=100, help="1 to 1000 (default 100)")
    args = parser.parse_args()

    board_id, short = trello.read_board(trello.board_ref(args))
    lists = trello.by_position(
        trello.get_array(
            "read lists",
            f"/boards/{trello.seg(board_id)}/lists",
            [("filter", "all"), ("fields", "name,closed,pos")],
        )
    )
    members = trello.board_members(board_id)
    labels = trello.board_labels(board_id)

    list_names = {trello.str_of(lst, "id"): trello.str_of(lst, "name") for lst in lists}
    member_names = {}
    usernames = []
    for member in members:
        id_, username = trello.str_of(member, "id"), trello.str_of(member, "username")
        if id_ and username:
            member_names[id_] = username
            usernames.append(username)
    known_labels = []
    for label in labels:
        name = trello.str_of(label, "name")
        if name and name not in known_labels:
            known_labels.append(name)

    closed_lists = {trello.str_of(lst, "id") for lst in lists if trello.flag_of(lst, "closed")}
    considered = [
        (trello.str_of(lst, "id"), trello.str_of(lst, "name"))
        for lst in lists
        if args.include_archived or trello.str_of(lst, "id") not in closed_lists
    ]
    in_lists = None
    if args.list is not None:
        in_lists = {trello.one_named("list", name, considered) for name in args.list}
    for name in (args.labels or []) + (args.without_labels or []):
        if name not in known_labels:
            raise trello.TrelloError(trello.missing("label", name, known_labels))
    on_cards = None
    if args.members is not None:
        on_cards = set()
        for who in args.members:
            if who == trello.ME:
                on_cards.add(trello.me()[0])
                continue
            found = [id_ for id_, username in member_names.items() if username == who]
            if not found:
                raise trello.TrelloError(trello.missing("member", who, usernames))
            on_cards.add(found[0])

    order = {trello.str_of(lst, "id"): at for at, lst in enumerate(lists)}
    kept = []
    for card in board_cards(board_id, args.include_archived):
        list_id = trello.str_of(card, "idList")
        if not args.include_archived and (
            trello.flag_of(card, "closed") or list_id in closed_lists
        ):
            continue
        if in_lists is not None and list_id not in in_lists:
            continue
        carried = trello.label_names(card)
        if any(name not in carried for name in args.labels or []):
            continue
        if any(name in carried for name in args.without_labels or []):
            continue
        if on_cards is not None:
            on = card.get("idMembers") if isinstance(card.get("idMembers"), list) else []
            if not any(m in on_cards for m in on if isinstance(m, str)):
                continue
        kept.append(card)
    kept.sort(
        key=lambda card: (order.get(trello.str_of(card, "idList"), len(order)), trello.position(card))
    )
    trello.emit(
        trello.only_handles(
            trello.card_handle(card, list_names, member_names, short) for card in kept[: args.limit]
        )
    )


if __name__ == "__main__":
    trello.run(main)
