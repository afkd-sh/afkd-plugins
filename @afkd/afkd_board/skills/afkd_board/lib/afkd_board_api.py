# afkd_board_api.py — what every script of the bundled `afkd_board` skill shares.
#
# Each script is one operation of the plugin under the operation's name (`move_card.py`,
# `cards.py`, …), the same operation the plugin's `afkd_board.move_card` and `afkd_board.cards`
# do over an `afkd_board.Board`: it parses the operation's parameters, does what the plugin does,
# and prints what the operation gives back, as JSON. This module holds the rest, once, each part
# a port of the plugin's own: the requests and their refusals (src/client.rs), the handles
# (src/reads.rs), list names turned into ids (src/writes.rs) and the run a change is made in
# (src/board.rs).
#
# THE ENVIRONMENT is the one a `service(afkd_board)` run carries (src/service.rs):
# AFKD_BOARD_URL, the board's address; AFKD_BOARD_AUTHORIZATION, the value of the `Authorization`
# header, `Bearer afk_board_…`; and AFKD_BOARD_RUN, the JSON of what the plugin knows of the run
# the agent is in: its `service`. A script that needs a variable that is unset exits 1 naming it,
# before any request.
#
# THE RUN. A change of a card, its checklist or a comment is shown on the board under the run it
# was made in, when the request names one. A run is its `machine`, its `service`, its `instance`
# and its `id`: the id is the leaf of $AFKD_SCRATCH_DIR, the instance is $AFKD_INSTANCE (else
# "0"), the service is AFKD_BOARD_RUN's, and the machine is AFKD_BOARD_RUN's `machine`, which
# afkd does not yet say: no variable a run carries names it. Until it does, no script sends a
# `run`, and says so once, on stderr, and the board shows the change under the key's name.
#
# WHAT A SCRIPT PRINTS is the value the plugin's reply carries: a handle as an object of its
# `id`, its `key` (`afkd_board:<board uuid>/<object uuid>`) and its `fields`, a list of them as
# an array, a struct (a list, a label, a checklist item) as an object, and nothing as `null`. A
# time is unix milliseconds. A refusal is the plugin's sentence on stderr after the script's
# name, and exit 1.
#
# Python 3.9 or newer, and the standard library only. Not executable: the scripts import it.

import argparse
import base64
import calendar
import http.client
import json
import os
import sys
import urllib.parse
import urllib.request

# --- the environment -------------------------------------------------------------------

ENV_URL = "AFKD_BOARD_URL"
ENV_AUTHORIZATION = "AFKD_BOARD_AUTHORIZATION"
ENV_RUN = "AFKD_BOARD_RUN"
ENV_SCRATCH_DIR = "AFKD_SCRATCH_DIR"
ENV_INSTANCE = "AFKD_INSTANCE"

# The script running, as its failures name it, and the operation it is.
SCRIPT = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "afkd_board"
ACTION = SCRIPT[:-3] if SCRIPT.endswith(".py") else SCRIPT

# How long one request may take.
TIMEOUT = 30

# The label a claimed card carries, which the service puts on and takes off: not for a script
# to touch, but named here so the skill's guide can say so.
CLAIMED_LABEL = "afkd/claimed"


class BoardError(Exception):
    """A request that did not do what it asked, as the one sentence that says why."""


def die(sentence, code=1):
    """Print `<script>: <sentence>` to stderr and exit `code`."""
    print(f"{SCRIPT}: {sentence}", file=sys.stderr)
    sys.exit(code)


def run(main):
    """Run a script's `main`, a BoardError ending it as its one-line failure, exit 1."""
    try:
        main()
    except BoardError as e:
        die(str(e))


def emit(value):
    """Print `value`, what the script's operation gives back, as JSON."""
    print(json.dumps(value, ensure_ascii=False, indent=2))


def need_env(*names):
    """The values of `names` from the environment, in order; exits 1 naming each one unset."""
    values = [os.environ.get(name) for name in names]
    missing = [name for name, value in zip(names, values) if not value]
    if missing:
        die(f"{' and '.join(missing)} must be set in the environment")
    return values


def read_text(path):
    """The UTF-8 text of the file at `path`; exits 1 naming it when it cannot be read."""
    try:
        with open(path, encoding="utf-8") as handle_:
            return handle_.read()
    except (OSError, UnicodeDecodeError) as e:
        die(f"cannot read {path}: {e}")


def truth(text):
    """An argparse type: `true` or `false`."""
    if text in ("true", "false"):
        return text == "true"
    raise argparse.ArgumentTypeError(f"{text!r} is `true` or `false`")


# --- the run a change is made in ---------------------------------------------------------

_said = False


def made_in():
    """The run a change is made in, as the board takes it: `machine`, `service`, `instance`
    and `id`; None when any is not known, which says why once on stderr. Outside a
    `service(afkd_board)` run (no AFKD_BOARD_RUN) there is no run, and nothing is said."""
    global _said
    carried = os.environ.get(ENV_RUN)
    if not carried:
        return None
    try:
        known = json.loads(carried)
    except ValueError:
        known = None
    if not isinstance(known, dict):
        die(f"{ENV_RUN} is not the JSON object a service(afkd_board) run carries")
    scratch = os.environ.get(ENV_SCRATCH_DIR, "").rstrip("/")
    run_ = {
        "machine": known.get("machine"),
        "service": known.get("service"),
        "instance": os.environ.get(ENV_INSTANCE) or "0",
        "id": os.path.basename(scratch) if scratch else None,
    }
    missing = [name for name, value in run_.items() if not isinstance(value, str) or not value]
    if missing:
        if not _said:
            _said = True
            print(
                f"{SCRIPT}: the change is made under the key's name, not this run: "
                f"{', '.join(missing)} of the run is not known here (afkd does not yet name "
                f"the run's machine in a variable)",
                file=sys.stderr,
            )
        return None
    return run_


def with_run(body):
    """`body`, an object, with the run it is made in under "run" when there is one."""
    made = made_in()
    if made is not None:
        body["run"] = made
    return body


# --- HTTP ------------------------------------------------------------------------------


def connection(url):
    """Open an HTTP(S) connection to the host of `url`, tunnelled through the proxy the
    environment names for its scheme, minus the hosts NO_PROXY lists: a sandbox with a network
    list reaches its names only through afkd's proxy (ADR-0084)."""
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


def seg(value):
    """`value` as one segment of a path: all but a letter, a digit and `-._~` escaped."""
    return urllib.parse.quote(str(value), safe="-._~")


def request(method, path, query=None, body=None):
    """One request under `<AFKD_BOARD_URL>/api/board`: the JSON it answers with, None for a 204
    or an empty body; or a BoardError saying why not, behind the operation's name."""
    base, authorization = need_env(ENV_URL, ENV_AUTHORIZATION)
    base = base.rstrip("/")
    root = urllib.parse.urlsplit(base)
    url = base + "/api/board" + path
    if query:
        url += "?" + urllib.parse.urlencode(query)
    headers = {"Authorization": authorization, "Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"
    target = urllib.parse.urlsplit(url)
    try:
        conn = connection(url)
        conn.request(method, target.path + (f"?{target.query}" if target.query else ""), data, headers)
        resp = conn.getresponse()
        raw = resp.read()
        status = resp.status
        conn.close()
    except (OSError, http.client.HTTPException) as e:
        raise BoardError(f"{ACTION}: cannot reach the board at {root.netloc}: {e}") from None
    text = raw.decode("utf-8", "replace")
    if not 200 <= status < 300:
        try:
            problem = json.loads(text)["error"]
            if not isinstance(problem, str):
                raise ValueError(text)
        except (ValueError, KeyError, TypeError):
            raise BoardError(f"{ACTION}: the board answered {status}") from None
        raise BoardError(f"{ACTION}: {problem}")
    if status == 204 or not text.strip():
        return None
    try:
        return json.loads(text)
    except ValueError as e:
        raise BoardError(
            f"{ACTION}: the board answered {status} with what it does not say: {e}"
        ) from None


# --- handles ---------------------------------------------------------------------------


def millis(text):
    """The unix milliseconds of the RFC 3339 time `text`, to the second as src/rfc3339.rs reads
    it; a BoardError for one that is no instant."""
    try:
        date, sep, rest = text.partition("T")
        if not sep:
            date, sep, rest = text.partition(" ")
        year, month, day = (int(part) for part in date.split("-", 2))
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
        second = int((seconds[0] if seconds else "0").split(".")[0])
        total = calendar.timegm((year, month, day, int(hour), int(minute), second)) - offset
        if total < 0:
            raise ValueError(text)
        return total * 1000
    except (ValueError, OverflowError):
        raise BoardError(
            f"{ACTION}: the board answered a time that is no instant, `{text}`"
        ) from None


def card_handle(board_id, card):
    """The `afkd_board.Card` handle of the card the board answered: its number as its id."""
    return {
        "id": str(card["number"]),
        "key": f"afkd_board:{board_id}/{card['id']}",
        "fields": {
            "number": card["number"],
            "title": card["title"],
            "description": card["description"],
            "list": card["list"],
            "labels": card["labels"],
            "archived": card["archived"],
            "done": card["done"],
            "blocked_by": card["blocked_by"],
            "blocking": card["blocking"],
            "blocked": card["blocked"],
            "url": card["url"],
            "created_at": millis(card["created_at"]),
            "updated_at": millis(card["updated_at"]),
        },
    }


def comment_handle(board_id, card, comment):
    """The `afkd_board.Comment` handle of a comment on `card`: its author the person who made
    it, never a key, and its url its card's."""
    made_in_run = comment.get("run")
    return {
        "id": comment["id"],
        "key": f"afkd_board:{board_id}/{comment['id']}",
        "fields": {
            "card": card["number"],
            "body": comment["body"],
            "author": None if comment["by_key"] else comment.get("by"),
            "run": made_in_run["id"] if made_in_run else None,
            "created_at": millis(comment["created_at"]),
            "url": card["url"],
        },
    }


# --- reads -----------------------------------------------------------------------------


def board():
    """The board: its id, its name, its lists in order and its labels."""
    return request("GET", "")


def read_card(number):
    """The card `number`, archived or not, as the board answers it."""
    return request("GET", f"/cards/{number}")


def no_list(name, lists):
    """The sentence refusing the list `name`, which the board lacks, naming those it has."""
    has = [f"`{l['name']}`" for l in lists]
    if not has:
        text = "none"
    elif len(has) == 1:
        text = has[0]
    else:
        text = f"{', '.join(has[:-1])} and {has[-1]}"
    return f"{ACTION}: The board has no list `{name}`; it has {text}."


def list_id(board_, name):
    """The id of the list named `name`, matched exactly; a BoardError naming the lists the
    board has for one it lacks."""
    for list_ in board_["lists"]:
        if list_["name"] == name:
            return list_["id"]
    raise BoardError(no_list(name, board_["lists"]))


# --- arguments every script of a card shares ---------------------------------------------


def add_card_arg(parser, help_text="the card's number"):
    """Give `parser` the card it acts on, by its number."""
    parser.add_argument("card", type=int, metavar="CARD", help=help_text)


def add_place_args(parser, after_help, after_type=int):
    """Give `parser` the place a card or a list goes to: `--at top|bottom`, or `--after`."""
    place = parser.add_mutually_exclusive_group()
    place.add_argument("--at", choices=("top", "bottom"), help="the top or the bottom")
    place.add_argument("--after", type=after_type, metavar="AFTER", help=after_help)


def place_of(args):
    """The place the flags name, as the board's API takes it: `{"at": …}` or `{"after": …}`; an
    empty object for neither."""
    if args.at is not None:
        return {"at": args.at}
    if args.after is not None:
        return {"after": args.after}
    return {}


def add_description_args(parser):
    """Give `parser` the description, inline or from a file."""
    text = parser.add_mutually_exclusive_group()
    text.add_argument("--description", metavar="TEXT", help="markdown")
    text.add_argument("--description_file", metavar="PATH", help="the description, from a file")


def description_of(args):
    """The description the flags give, read from its file when it is one; None for neither."""
    if args.description_file is not None:
        return read_text(args.description_file)
    return args.description
