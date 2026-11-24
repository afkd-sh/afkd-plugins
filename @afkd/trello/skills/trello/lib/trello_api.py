# trello_api.py — what every script of the bundled `trello` skill shares.
#
# Each script is one Trello operation under the operation's name (`update_card.py`, `cards.py`,
# …), the same operation the plugin's `trello.update_card` and `trello.cards` do over a
# `trello.Account`: it parses the operation's parameters, does what the plugin does, and prints
# what the operation gives back, as JSON. This module holds the rest, once, each part a port of
# the plugin's own: the requests and their refusals (src/client.rs, src/http.rs), the handles
# (src/handles.rs), a card read back (src/client.rs's `read_card_handle`), names turned into
# ids (src/reads.rs's `one_named`), the limits on text and files (src/writes.rs,
# src/attachment.rs), and the file's download.
#
# THE ENVIRONMENT is the one a `service(trello)` run carries (src/common.rs): TRELLO_API_KEY
# and TRELLO_TOKEN, the credential; TRELLO_CARD_ID, the active card, which `--card` overrides;
# and TRELLO_BOARD_ID, its board, which `--board` overrides. TRELLO_API_BASE is the seam the
# tests point at a fake Trello: the host (and port) every request goes to, `https://api.trello.com`
# by default. A script that needs a variable that is unset exits 1 naming it, before any request.
#
# EVERY REQUEST carries the credential in the query, as the plugin's own do, and a JSON body
# when it has one, so no free text rides a request line. A refusal is the plugin's own
# sentence, `trello <stage>: board returned status <n>`, and on a write Trello's own words after
# it. The connection is the proxy-aware one a sandbox with a network list needs.
#
# WHAT A SCRIPT PRINTS is the value the plugin's reply carries: a handle as an object of its
# `id`, its `key` (`trello:<Trello's id>`, an attachment's and a check item's
# `trello:<card id>/<id>`) and its `fields`, a list of them as an array, and nothing as `null`.
# A Card has no `title`: its `name` holds the same text. Times are unix milliseconds. A
# refusal is the plugin's sentence on stderr after the script's name, and exit 1.
#
# Python 3.9 or newer, and the standard library only. Not executable: the scripts import it.

import argparse
import base64
import calendar
import contextlib
import http.client
import json
import os
import pathlib
import stat
import sys
import time
import urllib.parse
import urllib.request

# --- the environment -------------------------------------------------------------------

ENV_API_KEY = "TRELLO_API_KEY"
ENV_TOKEN = "TRELLO_TOKEN"
ENV_CARD_ID = "TRELLO_CARD_ID"
ENV_BOARD_ID = "TRELLO_BOARD_ID"
ENV_API_BASE = "TRELLO_API_BASE"

# Where Trello's API is, as TRELLO_API_BASE leaves it; the requests' paths start `/1`.
DEFAULT_BASE = "https://api.trello.com"
API_ROOT = "/1"

# The script running, as its failures name it.
SCRIPT = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "trello"

# How long one request may take.
TIMEOUT = 30

# The most characters Trello takes in a card's description or a comment.
MAX_TEXT = 16_384

# The most one attachment may hold, either way: src/attachment.rs's MAX_ATTACHMENT_BYTES.
MAX_ATTACHMENT_BYTES = 10 * 1024 * 1024

# The latest `due`, in unix milliseconds, that is a date of four digits: 9999-12-31.
MAX_DUE_MS = 253_402_300_799_999

# The most either read returns, and Trello's own page.
MAX_LIMIT = 1000

# The one member `me` names: whoever the credential acts as.
ME = "me"

# The card fields a board read asks for, and the attachment fields a read asks for.
CARD_FIELDS = (
    "name,desc,idList,idMembers,labels,shortLink,url,closed,due,dueComplete,"
    "idShort,dateLastActivity,pos"
)
ATTACHMENT_FIELDS = "id,name,date,url,isUpload,bytes,mimeType"


class TrelloError(Exception):
    """A request that did not do what it asked, as the one sentence that says why."""


def die(sentence, code=1):
    """Print `<script>: <sentence>` to stderr and exit `code`."""
    print(f"{SCRIPT}: {sentence}", file=sys.stderr)
    sys.exit(code)


def run(main):
    """Run a script's `main`, a TrelloError ending it as its one-line failure, exit 1."""
    try:
        main()
    except TrelloError as e:
        die(str(e))


def emit(value):
    """Print `value`, what the script's operation gives back, as JSON."""
    print(json.dumps(value, ensure_ascii=False, indent=2))


def need_env(*names):
    """The values of `names` from the environment, in order; exits 1 naming each one unset."""
    values = [os.environ.get(name) for name in names]
    missing_names = [name for name, value in zip(names, values) if not value]
    if missing_names:
        die(f"{' and '.join(missing_names)} must be set in the environment")
    return values


def add_card_arg(parser):
    """Give `parser` the `--card` override of the active card."""
    parser.add_argument(
        "--card",
        metavar="ID",
        help=f"the card, by id or short link (defaults to ${ENV_CARD_ID})",
    )


def card_ref(args):
    """The card the script acts on: `--card`, else `$TRELLO_CARD_ID`; exits 1 with neither."""
    ref = args.card or os.environ.get(ENV_CARD_ID)
    if not ref:
        die(f"no card (set {ENV_CARD_ID} or pass --card)")
    return ref


def add_board_arg(parser):
    """Give `parser` the `--board` override of the active board."""
    parser.add_argument(
        "--board",
        metavar="ID",
        help=f"the board, by address, id or short link (defaults to ${ENV_BOARD_ID})",
    )


def board_ref(args):
    """The board the script acts on, reduced to the id its address carries: `--board`, else
    `$TRELLO_BOARD_ID`; exits 1 with neither."""
    ref = args.board or os.environ.get(ENV_BOARD_ID)
    if not ref:
        die(f"no board (set {ENV_BOARD_ID} or pass --board)")
    return board_id_of(ref.strip())


def board_id_of(address):
    """The segment after `/b/` in a board's address, else `address` itself: src/settings.rs's
    `derive_board_id`."""
    rest = address.split("/b/")
    if len(rest) > 1:
        segment = rest[1].split("/")[0]
        if segment:
            return segment
    return address


def refuse_placeholder(text, placeholder):
    """Exit 2 when `text` is SKILL.md's placeholder, before any request."""
    if text == placeholder:
        die("refusing to send the placeholder text; substitute your own", 2)


def read_text(path):
    """The UTF-8 text of the file at `path`; exits 1 naming it when it cannot be read."""
    try:
        with open(path, encoding="utf-8") as handle_:
            return handle_.read()
    except (OSError, UnicodeDecodeError) as e:
        die(f"cannot read {path}: {e}")


def quoted(text):
    """`text` as Rust's `{:?}` writes a string: in double quotes, `"` and `\\` escaped, and
    each control character escaped as Rust escapes it."""
    escapes = {'"': '\\"', "\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t", "\0": "\\0"}
    out = []
    for c in text:
        if c in escapes:
            out.append(escapes[c])
        elif ord(c) < 0x20 or 0x7F <= ord(c) <= 0x9F:
            out.append(f"\\u{{{ord(c):x}}}")
        else:
            out.append(c)
    return '"' + "".join(out) + '"'


def within(param, text):
    """`text`, the flag or argument `param`, or the refusal for one past Trello's 16 384
    characters, counted as Trello counts, in characters and not bytes."""
    length = len(text)
    if length > MAX_TEXT:
        raise TrelloError(f"{param} is {length} characters, over the {MAX_TEXT} Trello takes")
    return text


# --- reading a script's arguments --------------------------------------------------------


def add_place_arg(parser, help_text="the top or the bottom"):
    """Give `parser` the `--at` of a card or an item: the top or the bottom."""
    parser.add_argument("--at", choices=("top", "bottom"), help=help_text)


def add_description_args(parser):
    """Give `parser` the card's description, inline or from a file."""
    text = parser.add_mutually_exclusive_group()
    text.add_argument("--description", metavar="TEXT", help="markdown")
    text.add_argument("--description_file", metavar="PATH", help="the description, from a file")


def description_of(args):
    """The description the flags give, read from its file when it is one, refused past
    Trello's limit; None when neither flag is given."""
    if args.description_file is not None:
        return within("description", read_text(args.description_file))
    if args.description is not None:
        return within("description", args.description)
    return None


def rfc3339(text):
    """An argparse type: a time as RFC 3339 writes one, as unix milliseconds."""
    try:
        ms = millis(text)
    except (ValueError, OverflowError, IndexError):
        raise argparse.ArgumentTypeError(
            f"{text!r} is no RFC 3339 time (2026-11-17T09:30:00Z)"
        ) from None
    if ms > MAX_DUE_MS:
        raise argparse.ArgumentTypeError(f"{text!r} is past 9999-12-31")
    return ms


def millis(text):
    """The unix milliseconds of the RFC 3339 time `text`, as src/rfc3339.rs's
    `parse_rfc3339_millis` reads it: digits past the third of a second dropped. Raises
    ValueError for one it cannot read."""
    date, sep, rest = text.partition("T")
    if not sep:
        date, sep, rest = text.partition(" ")
    year, month, day = (int(part) for part in date.split("-", 2))
    if not 1 <= month <= 12 or not 1 <= day <= 31:
        raise ValueError(text)
    offset = 0
    if rest.endswith(("Z", "z")):
        rest = rest[:-1]
    else:
        for at, c in enumerate(rest):
            if c in "+-" and at > 0:
                hours, _, minutes = rest[at + 1 :].partition(":")
                offset = (1 if c == "+" else -1) * (int(hours) * 3600 + int(minutes or 0) * 60)
                rest = rest[:at]
                break
    hour, minute, *seconds = rest.split(":", 2)
    second, dot, fraction = (seconds[0] if seconds else "0").partition(".")
    if int(hour) > 23 or int(minute) > 59 or int(second) > 60:
        raise ValueError(text)
    if dot and not (fraction.isascii() and fraction.isdigit()):
        raise ValueError(text)
    days = calendar.timegm((year, month, day, 0, 0, 0)) // 86_400
    total = days * 86_400 + int(hour) * 3600 + int(minute) * 60 + int(second) - offset
    if total < 0:
        raise ValueError(text)
    return total * 1000 + (int((fraction + "00")[:3]) if fraction else 0)


def iso(ms):
    """`ms`, unix milliseconds, as Trello writes a date: `YYYY-MM-DDTHH:MM:SS.mmmZ`."""
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(ms // 1000)) + f".{ms % 1000:03d}Z"


# --- HTTP ------------------------------------------------------------------------------


def connection(url):
    """Open an HTTP(S) connection to the host of `url`, the API's or a download's, tunnelled
    through the proxy the environment names for its scheme, minus the hosts NO_PROXY lists: a
    sandbox with a network list reaches its names only through afkd's proxy (ADR-0084)."""
    parts = urllib.parse.urlsplit(url)
    https = parts.scheme == "https"
    kind = http.client.HTTPSConnection if https else http.client.HTTPConnection
    proxy = urllib.request.getproxies().get(parts.scheme)
    if not proxy or urllib.request.proxy_bypass(parts.hostname):
        return kind(parts.hostname, parts.port, timeout=TIMEOUT)
    via = urllib.parse.urlsplit(proxy)
    conn = kind(via.hostname, via.port, timeout=TIMEOUT)
    headers = {}
    if via.username is not None:
        user = urllib.parse.unquote(via.username)
        password = urllib.parse.unquote(via.password or "")
        token = base64.b64encode(f"{user}:{password}".encode()).decode()
        headers["Proxy-Authorization"] = f"Basic {token}"
    conn.set_tunnel(parts.hostname, parts.port or (443 if https else 80), headers=headers)
    return conn


def api_base():
    """Where the API is: TRELLO_API_BASE, else Trello's."""
    return os.environ.get(ENV_API_BASE) or DEFAULT_BASE


def seg(value):
    """`value` as one segment of a path."""
    return urllib.parse.quote(str(value), safe="")


def answer_text(data):
    """Trello's words in a refusal's body: a JSON string unquoted, a JSON object's `message`,
    else the body as it came, trimmed; "" when it holds none."""
    text = data.decode("utf-8", "replace")
    try:
        found = json.loads(text)
    except ValueError:
        found = text
    if isinstance(found, str):
        text = found
    elif isinstance(found, dict) and isinstance(found.get("message"), str):
        text = found["message"]
    return text.strip()


def undecodable(stage, reason):
    """The error of a reply that cannot be read as the shape asked for."""
    return TrelloError(f"trello {stage}: undecodable response ({reason})")


def multipart(name, content):
    """The `multipart/form-data` body Trello's upload takes, and its content type: a `name`
    part carrying the attachment's name verbatim, and a `file` part carrying the bytes
    verbatim under an ASCII-safe copy of the name. src/attachment.rs's `multipart`."""
    named = name.encode("utf-8")
    boundary = next(
        b
        for b in (f"afkd-trello-{n}" for n in range(1 << 31))
        if f"--{b}".encode() not in named and f"--{b}".encode() not in content
    )
    safe = "".join(c if (c.isascii() and c.isalnum()) or c in "._+-" else "_" for c in name)
    body = (
        f'--{boundary}\r\nContent-Disposition: form-data; name="name"\r\n\r\n{name}\r\n'
        f'--{boundary}\r\nContent-Disposition: form-data; name="file"; '
        f'filename="{safe or "attachment"}"\r\nContent-Type: application/octet-stream\r\n\r\n'
    ).encode("utf-8")
    body += content + f"\r\n--{boundary}--\r\n".encode()
    return f"multipart/form-data; boundary={boundary}", body


def request(stage, method, path, pairs=(), body=None, upload=None):
    """Send one request to the API and return its reply as JSON (None for an empty one).

    The credential rides the query beside `pairs`. `body` is sent as JSON, `upload`, a
    `(name, bytes)`, as a multipart form. Raises TrelloError with the plugin's sentence,
    `trello <stage>: <why>`, for a non-2xx, naming Trello's own words on a write; for a
    transport failure; and for a reply that is no JSON.
    """
    key, token = need_env(ENV_API_KEY, ENV_TOKEN)
    query = urllib.parse.urlencode([("key", key), ("token", token), *pairs])
    headers, payload = {}, None
    if upload is not None:
        content_type, payload = multipart(*upload)
        headers["Content-Type"] = content_type
    elif body is not None:
        payload = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    conn = connection(api_base())
    try:
        conn.request(method, f"{API_ROOT}{path}?{query}", payload, headers)
        response = conn.getresponse()
        status, data = response.status, response.read()
    except (OSError, http.client.HTTPException) as e:
        raise TrelloError(f"trello {stage}: no response ({e})") from None
    finally:
        conn.close()
    if not 200 <= status < 300:
        words = answer_text(data) if method != "GET" else ""
        suffix = f": {words}" if words else ""
        raise TrelloError(f"trello {stage}: board returned status {status}{suffix}")
    if not data.strip():
        return None
    try:
        return json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as e:
        raise undecodable(stage, e) from None


def get_array(stage, path, pairs=()):
    """`path` read as a JSON array."""
    found = request(stage, "GET", path, pairs)
    if not isinstance(found, list):
        raise undecodable(stage, "expected an array")
    return found


def read_back(what, read):
    """What `read()` gives, the read-back of what a write made or changed; else the sentence
    saying the write landed and its read-back did not."""
    try:
        return read()
    except TrelloError as e:
        raise TrelloError(f"the {what} was written, but reading it back failed: {e}") from None


# --- the handles: ports of src/handles.rs ----------------------------------------------


def str_of(value, key):
    """The string at `key` of `value`, "" when it has none."""
    text = value.get(key) if isinstance(value, dict) else None
    return text if isinstance(text, str) else ""


def some_str(value, key):
    """The string at `key` of `value`, None when it is empty or not there."""
    return str_of(value, key) or None


def flag_of(value, key):
    """The bool at `key` of `value`, False when it has none."""
    return value.get(key) is True if isinstance(value, dict) else False


def key_of(*ids):
    """The key of the object Trello knows as `ids`: `trello:<id>`, `trello:<card id>/<id>`."""
    return "trello:" + "/".join(ids)


def time_of(value, key):
    """The time the string at `key` of `value` holds, in unix milliseconds; None for none."""
    text = value.get(key) if isinstance(value, dict) else None
    if not isinstance(text, str):
        return None
    try:
        return millis(text)
    except (ValueError, OverflowError, IndexError):
        return None


def created_of(id_):
    """When the object Trello knows as `id_` was made: the second its ObjectId begins with, in
    unix milliseconds; 0 for an id that is no ObjectId."""
    try:
        return int(id_[:8], 16) * 1000 if len(id_) >= 8 else 0
    except ValueError:
        return 0


def count_of(value, key):
    """The non-negative whole number at `key` of `value`, None for none."""
    number = value.get(key) if isinstance(value, dict) else None
    if isinstance(number, int) and not isinstance(number, bool) and number >= 0:
        return number
    return None


def position(value):
    """Trello's `pos` of `value`, past every other when it has none."""
    pos = value.get("pos") if isinstance(value, dict) else None
    if isinstance(pos, (int, float)) and not isinstance(pos, bool):
        return float(pos)
    return sys.float_info.max


def by_position(items):
    """`items` ordered by Trello's `pos`, an item with none last and Trello's own order
    among equals."""
    return sorted(items, key=position)


def handle(id_, key, fields):
    """A handle: its `id`, its `key` and its `fields`."""
    return {"id": id_, "key": key, "fields": fields}


def label_names(card):
    """The names of a card's labels; a colour-only label is left out."""
    labels = card.get("labels") if isinstance(card, dict) else None
    names = [str_of(label, "name") for label in labels] if isinstance(labels, list) else []
    return [name for name in names if name]


def nested_lookups(card):
    """What a card read with its list, board and members nested names: `(lists, members,
    board)`, a list id → name, a member id → username, and the board's short link."""
    lst = card.get("list") if isinstance(card.get("list"), dict) else {}
    lists = {}
    if isinstance(lst.get("id"), str) and isinstance(lst.get("name"), str):
        lists[lst["id"]] = lst["name"]
    members = {}
    nested = card.get("members") if isinstance(card.get("members"), list) else []
    for member in nested:
        if isinstance(member, dict) and isinstance(member.get("id"), str):
            if isinstance(member.get("username"), str):
                members[member["id"]] = member["username"]
    return lists, members, str_of(card.get("board"), "shortLink")


def card_handle(card, lists, members, board):
    """The card `card` as a `trello.Card` handle: its `id` the short link, its key
    `trello:<card id>`. `lists` and `members` are the names of the ids the card holds, `board`
    the board's short link. None for JSON that holds no card id. A Card has no `title`."""
    id_ = card.get("id") if isinstance(card, dict) else None
    if not isinstance(id_, str):
        return None
    created = created_of(id_)
    ids = card.get("idMembers") if isinstance(card.get("idMembers"), list) else []
    last = time_of(card, "dateLastActivity")
    number = card.get("idShort")
    fields = {
        "number": number if isinstance(number, int) and not isinstance(number, bool) else 0,
        "name": str_of(card, "name"),
        "url": str_of(card, "url"),
        "description": str_of(card, "desc"),
        "board": board,
        "list": lists.get(str_of(card, "idList"), ""),
        "labels": label_names(card),
        "members": [members[m] for m in ids if isinstance(m, str) and m in members],
        "due": time_of(card, "due"),
        "due_complete": flag_of(card, "dueComplete"),
        "archived": flag_of(card, "closed"),
        "created_at": created,
        "last_activity_at": created if last is None else last,
    }
    return handle(some_str(card, "shortLink") or id_, key_of(id_), fields)


def comment_handle(action, card):
    """The comment action `action`, on the card whose short link is `card`, as a handle.
    None for an action with no id or text."""
    id_ = action.get("id") if isinstance(action, dict) else None
    data = action.get("data") if isinstance(action, dict) else None
    if not isinstance(id_, str) or not isinstance(data, dict):
        return None
    if not isinstance(data.get("text"), str):
        return None
    dated = time_of(action, "date")
    fields = {
        "card": card,
        "body": data["text"],
        "author": some_str(action.get("memberCreator"), "username"),
        "created_at": created_of(id_) if dated is None else dated,
        "edited_at": time_of(data, "dateLastEdited"),
    }
    return handle(id_, key_of(id_), fields)


def ordered_items(checklist):
    """The items of `checklist` in order: by `pos`, an item with none last."""
    items = checklist.get("checkItems") if isinstance(checklist, dict) else None
    if not isinstance(items, list):
        return []
    return by_position([item for item in items if isinstance(item, dict)])


def checklist_handle(checklist, card):
    """The checklist `checklist` of the card whose short link is `card`, as a handle: its
    items the names of its check items in order. None for JSON with no id."""
    id_ = checklist.get("id") if isinstance(checklist, dict) else None
    if not isinstance(id_, str):
        return None
    fields = {
        "card": card,
        "name": str_of(checklist, "name"),
        "items": [str_of(item, "name") for item in ordered_items(checklist)],
    }
    return handle(id_, key_of(id_), fields)


def check_item_handles(checklist, card_id, card):
    """The check items of `checklist` of the card `card_id`, whose short link is `card`, in
    order, each as a handle: its key carries the card's id too."""
    list_id = str_of(checklist, "id")
    built = []
    for item in ordered_items(checklist):
        id_ = item.get("id")
        if isinstance(id_, str):
            fields = {
                "card": card,
                "checklist": list_id,
                "name": str_of(item, "name"),
                "done": str_of(item, "state") == "complete",
            }
            built.append(handle(id_, key_of(card_id, id_), fields))
    return built


def attachment_handle(attachment, card_id, card):
    """The attachment `attachment` of the card `card_id`, whose short link is `card`, as a
    handle: its key carries the card's id too. None for JSON with no id."""
    id_ = attachment.get("id") if isinstance(attachment, dict) else None
    if not isinstance(id_, str):
        return None
    dated = time_of(attachment, "date")
    fields = {
        "card": card,
        "name": str_of(attachment, "name"),
        "url": str_of(attachment, "url"),
        "bytes": count_of(attachment, "bytes"),
        "mime_type": some_str(attachment, "mimeType"),
        "created_at": created_of(id_) if dated is None else dated,
        "is_upload": flag_of(attachment, "isUpload"),
    }
    return handle(id_, key_of(card_id, id_), fields)


def board_handle(board):
    """The board `board` as a handle: its `id` the short link, its key `trello:<board id>`.
    None for JSON with no id or short link."""
    id_, short = str_of(board, "id"), str_of(board, "shortLink")
    if not id_ or not short:
        return None
    fields = {
        "name": str_of(board, "name"),
        "url": str_of(board, "url"),
        "archived": flag_of(board, "closed"),
    }
    return handle(short, key_of(id_), fields)


def list_handle(lst, board):
    """The list `lst` of the board whose short link is `board`, as a handle."""
    id_ = str_of(lst, "id")
    if not id_:
        return None
    fields = {"name": str_of(lst, "name"), "board": board, "archived": flag_of(lst, "closed")}
    return handle(id_, key_of(id_), fields)


def label_handle(label, board):
    """The label `label` of the board whose short link is `board`, as a handle. A label with
    no name has "", and one with no colour null."""
    id_ = str_of(label, "id")
    if not id_:
        return None
    fields = {
        "name": str_of(label, "name"),
        "color": some_str(label, "color"),
        "board": board,
    }
    return handle(id_, key_of(id_), fields)


def member_handle(member):
    """The member `member` as a handle: its `id` the username, its key `trello:<member id>`.
    None for JSON with no id or username."""
    id_, username = str_of(member, "id"), str_of(member, "username")
    if not id_ or not username:
        return None
    return handle(username, key_of(id_), {"full_name": str_of(member, "fullName")})


def only_handles(built):
    """The handles of `built`, each `None` (JSON that holds no handle) left out."""
    return [h for h in built if h is not None]


# --- reads: ports of src/client.rs and src/reads.rs ------------------------------------


def read_card(card):
    """The card `card`, by id or short link, as the `trello.Card` handle it is now: its list's
    name, its board's short link and its members' usernames resolved in the one read,
    `TrelloClient::read_card_handle`'s."""
    found = request(
        "read card",
        "GET",
        f"/cards/{seg(card)}",
        [
            ("list", "true"),
            ("list_fields", "name"),
            ("board", "true"),
            ("board_fields", "shortLink"),
            ("members", "true"),
            ("member_fields", "username"),
        ],
    )
    built = card_handle(found, *nested_lookups(found)) if isinstance(found, dict) else None
    if built is None:
        raise undecodable("read card", "card missing id")
    return built


def card_ids(card):
    """The id and the short link of the card `card`, by one read."""
    found = request("read card", "GET", f"/cards/{seg(card)}", [("fields", "shortLink")])
    id_ = str_of(found, "id")
    if not id_:
        raise undecodable("read card", "card missing id")
    return id_, str_of(found, "shortLink") or id_


def read_board(address):
    """The board `address` names, by address or short link: `(id, short link)`. A board the
    account has not is refused."""
    try:
        found = request(
            "read board",
            "GET",
            f"/boards/{seg(address)}",
            [("fields", "name,url,shortLink,closed")],
        )
    except TrelloError as e:
        if "returned status 404" in str(e):
            raise TrelloError(f"no board {quoted(address)} on this account: {e}") from None
        raise
    id_ = str_of(found, "id")
    if not id_:
        raise undecodable("read board", "board has no id")
    return id_, str_of(found, "shortLink") or id_


def board_labels(board_id):
    """Every label of a board: Trello gives 50 unless asked, so the read asks for the 1000 it
    allows."""
    return get_array(
        "read labels",
        f"/boards/{seg(board_id)}/labels",
        [("limit", str(MAX_LIMIT)), ("fields", "name,color")],
    )


def board_members(board_id):
    """A board's members."""
    return get_array(
        "read members", f"/boards/{seg(board_id)}/members", [("fields", "username,fullName")]
    )


def open_lists(board_id):
    """The open lists of a board."""
    return get_array(
        "read lists",
        f"/boards/{seg(board_id)}/lists",
        [("filter", "open"), ("fields", "name")],
    )


def checklists_of(card_id):
    """A card's checklists with their items, in the card's order."""
    return by_position(
        get_array(
            "read checklists",
            f"/cards/{seg(card_id)}/checklists",
            [("checkItems", "all"), ("fields", "name,pos")],
        )
    )


def me():
    """The member the credential acts as: `(id, username, full name)`."""
    found = request("read me", "GET", "/members/me")
    id_ = str_of(found, "id")
    if not id_:
        raise undecodable("read me", "authed member missing id")
    return id_, str_of(found, "username"), str_of(found, "fullName")


def read_comment(id_):
    """The comment action `id_` as a handle, the card its short link."""
    found = request(
        "read comment",
        "GET",
        f"/actions/{seg(id_)}",
        [("memberCreator_fields", "username,fullName")],
    )
    data = found.get("data") if isinstance(found, dict) else None
    on = data.get("card") if isinstance(data, dict) else None
    built = comment_handle(found, str_of(on, "shortLink") or str_of(on, "id"))
    if built is None:
        raise undecodable("read comment", "no text")
    return built


def read_check_item(card_id, item_id):
    """The check item `item_id` of the card `card_id`, read with the card's checklists in
    the one request, as a handle keyed by its card."""
    found = request(
        "read card",
        "GET",
        f"/cards/{seg(card_id)}",
        [("fields", "shortLink"), ("checklists", "all"), ("checkItems", "all")],
    )
    short = str_of(found, "shortLink") or card_id
    lists = found.get("checklists") if isinstance(found, dict) else None
    for lst in lists if isinstance(lists, list) else []:
        for item in check_item_handles(lst, card_id, short):
            if item["id"] == item_id:
                return item
    raise undecodable("read card", f"no check item {item_id}")


def candidates(items, key):
    """`(id, name)` of each of `items` with a non-empty string `key`: the candidates a name is
    matched against."""
    found = []
    for item in items:
        id_, name = str_of(item, "id"), str_of(item, key)
        if id_ and name:
            found.append((id_, name))
    return found


def listed(names):
    """The names `names` lists, each quoted, for a sentence naming what a board has."""
    if not names:
        return "none"
    return ", ".join(quoted(name) for name in names)


def missing(kind, name, have):
    """The sentence refusing a `name` of `kind` the board has none of, naming the ones it has."""
    return f"the board has no {kind} named {quoted(name)}; its {kind}s are {listed(have)}"


def one_named(kind, name, found):
    """The id of the one of `found`, `(id, name)` pairs of `kind`, that is named `name`.
    Raises the sentence for a name none of them has, naming the ones there are, and for one
    several share, listing their ids: src/reads.rs's `one_named`."""
    named = [id_ for id_, candidate in found if candidate == name]
    if len(named) == 1:
        return named[0]
    if not named:
        have = []
        for _, candidate in found:
            if candidate not in have:
                have.append(candidate)
        raise TrelloError(missing(kind, name, have))
    raise TrelloError(
        f"{len(named)} {kind}s are named {quoted(name)} (ids {', '.join(named)}); "
        "name one of them by renaming the other"
    )


def named_or_id(kind, ref, items):
    """The id of the one of `items`, a board's lists or labels, that `ref` names: by its id,
    or by its exact name."""
    if any(str_of(item, "id") == ref for item in items):
        return ref
    return one_named(kind, ref, candidates(items, "name"))


def username_of(who):
    """The username `who` names: the member the credential acts as for `me`."""
    return me()[1] if who == ME else who


# --- files: ports of src/attachment.rs -------------------------------------------------


def read_upload(path, name):
    """The bytes of the file at `path`, to be put on a card as `name`: `(name, bytes)`.
    Raises the sentence refusing a relative path, an empty name, a path that is not a regular
    file, one that cannot be read, or a file over 10 MiB."""
    if not os.path.isabs(path):
        raise TrelloError(f"path must be absolute, not {quoted(path)}")
    if not name:
        raise TrelloError("name must not be empty")
    try:
        mode = os.stat(path).st_mode
        if not stat.S_ISREG(mode):
            raise TrelloError(f"{path} is not a regular file")
        with open(path, "rb") as handle_:
            # One byte past the cap, not the size the metadata gave: a file still being
            # written can grow between the two.
            content = handle_.read(MAX_ATTACHMENT_BYTES + 1)
    except FileNotFoundError:
        raise TrelloError(f"no file at {path}") from None
    except OSError as e:
        raise TrelloError(f"cannot read {path}: {e}") from None
    if len(content) > MAX_ATTACHMENT_BYTES:
        raise TrelloError(
            f"{path} is over the {MAX_ATTACHMENT_BYTES // (1024 * 1024)} MiB "
            "Trello takes in one attachment"
        )
    return name, content


def origin(url):
    """The scheme, host and port of `url`, lowercased: what decides where a credential may
    go; None when it names no scheme and host."""
    parts = urllib.parse.urlsplit(url)
    try:
        port = parts.port
    except ValueError:
        return None
    if not parts.scheme or not parts.hostname:
        return None
    return parts.scheme.lower(), parts.hostname.lower(), port


def download(url):
    """The bytes at the attachment `url`, no more than 10 MiB of them.

    The url is the listing's, verbatim, fetched with the OAuth `Authorization` header Trello's
    download host asks for and no credential in the query, which that host refuses. The
    credential goes only where the plugin sends it: Trello's own host and the API's. A url
    elsewhere is refused before it is asked.
    """
    key, token = need_env(ENV_API_KEY, ENV_TOKEN)
    at = origin(url)
    if at is None or at not in (origin("https://trello.com"), origin(api_base())):
        host = at[1] if at else "no host"
        raise TrelloError(
            f"trello download attachment: the attachment is hosted at {host}, which the "
            "plugin sends no credentials to"
        )
    parts = urllib.parse.urlsplit(url)
    target = parts.path + (f"?{parts.query}" if parts.query else "")
    header = {"Authorization": f'OAuth oauth_consumer_key="{key}", oauth_token="{token}"'}
    conn = connection(url)
    try:
        conn.request("GET", target, headers=header)
        response = conn.getresponse()
        if not 200 <= response.status < 300:
            raise TrelloError(
                f"trello download attachment: board returned status {response.status}"
            )
        content = response.read(MAX_ATTACHMENT_BYTES + 1)
    except (OSError, http.client.HTTPException) as e:
        raise TrelloError(f"trello download attachment: no response ({e})") from None
    finally:
        conn.close()
    if len(content) > MAX_ATTACHMENT_BYTES:
        raise undecodable(
            "download attachment", f"the body is over {MAX_ATTACHMENT_BYTES} bytes"
        )
    return content


def write_atomic(dest, content):
    """Write `content` to `dest` through a sibling temp file renamed onto it, so `dest` never
    holds part of a download. A missing parent directory is an error, and so is a path that
    names no file. Raises OSError."""
    directory, base = os.path.split(dest)
    if not base:
        raise OSError("the path names no file")
    temp = os.path.join(directory, f".{base}.afkd-trello-{os.getpid()}")
    try:
        with open(temp, "wb") as handle_:
            handle_.write(content)
        os.replace(temp, dest)
    except OSError:
        with contextlib.suppress(OSError):
            pathlib.Path(temp).unlink()
        raise
