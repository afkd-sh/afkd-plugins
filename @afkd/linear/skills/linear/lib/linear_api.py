# linear_api.py — what every script of the bundled `linear` skill shares.
#
# Each script is one of the plugin's actions (`linear.issue`, `linear.update_issue`, …) under
# the action's name: it parses the action's parameters, does what the plugin does, and prints
# what the action gives back, as JSON. This module holds the rest, once, each part a port of
# the plugin's own: the requests and their paging (src/client.rs), an issue read whole and the
# handle it is handed back as (src/wire.rs's IssueFields), the `issues` filter
# (src/reads.rs), names turned into ids (src/lookup.rs), the writes and the place an issue goes
# in its state (src/writes.rs), and the files an issue links and puts up (src/file.rs).
#
# THE ENVIRONMENT is the one the plugin puts on every run (src/common.rs): LINEAR_API_URL, the
# GraphQL endpoint; LINEAR_AUTHORIZATION, the exact `Authorization` value (an API key as it
# is, or `Bearer <token>` for an app); LINEAR_ISSUE_ID, the active issue's id; and
# LINEAR_ISSUE, its identifier (`ENG-123`). LINEAR_API_URL is also the seam the tests point at
# a fake Linear. A script that needs one that is unset exits 1 naming it, before any request.
# The credential reaches one workspace, so no script takes the action's `ws`: the run's
# workspace is the one every script reaches.
#
# EVERY REQUEST is one named operation out of operations.graphql beside this file, sent by
# its name, as the plugin sends its own: no script builds a GraphQL string. Linear answers a
# request it refuses with an `errors` body, on a 200 as well as on a 4xx, so every reply is
# read for `errors` before its `data`. A rate limit is said with the wait Linear asked for and
# never retried: the agent surfaces it.
#
# WHAT A SCRIPT PRINTS is the value the plugin's reply carries: a handle as an object of its
# `id`, its `key` (`linear:<organization uuid>/<object uuid>`) and its `fields`, a list of
# them as an array, `files`'s urls as an array of strings, and nil as `null`. A refusal is
# the plugin's own sentence on stderr, after the script's name, and exit 1.
#
# THE FILE HALF ports src/file.rs, function for function, and src/file.rs's parity test holds
# the two to one fixture. Rust counts a string in UTF-8 bytes and Python in characters, so
# each bound is measured in bytes here, and the two character classes Python spells otherwise
# are spelled out as Rust defines them.
#
# Python 3.9 or newer, and the standard library only. Not executable: the scripts import it.

import argparse
import calendar
import http.client
import json
import math
import os
import string
import sys
import time
import urllib.error
import urllib.request

# --- the environment -------------------------------------------------------------------

ENV_API_URL = "LINEAR_API_URL"
ENV_AUTHORIZATION = "LINEAR_AUTHORIZATION"
ENV_ISSUE_ID = "LINEAR_ISSUE_ID"
ENV_ISSUE = "LINEAR_ISSUE"

# The script running, as its failures name it.
SCRIPT = os.path.basename(sys.argv[0]) if sys.argv and sys.argv[0] else "linear"

# How long one request may take, the plugin's own read timeout.
TIMEOUT = 30

# Where the GraphQL endpoint sits under the API root, as LINEAR_API_URL ends.
GRAPHQL_PATH = "/graphql"


class LinearError(Exception):
    """A request that did not do what it asked, as the one sentence that says why."""


class NotFound(LinearError):
    """What a request named does not exist: Linear's `Entity not found`."""


def die(sentence, code=1):
    """Print `<script>: <sentence>` to stderr and exit `code`."""
    print(f"{SCRIPT}: {sentence}", file=sys.stderr)
    sys.exit(code)


def run(main):
    """Run a script's `main`, a LinearError ending it as its one-line failure, exit 1."""
    try:
        main()
    except LinearError as e:
        die(str(e))


def emit(value):
    """Print `value`, what the script's action gives back, as JSON."""
    print(json.dumps(value, ensure_ascii=False, indent=2))


def need_env(*names):
    """The values of `names` from the environment, in order; exits 1 naming each one unset."""
    values = [os.environ.get(name) for name in names]
    missing = [name for name, value in zip(names, values) if not value]
    if missing:
        die(f"{' and '.join(missing)} must be set in the environment")
    return values


def add_issue_arg(parser):
    """Give `parser` the `--issue` override of the active issue."""
    parser.add_argument(
        "--issue",
        metavar="REF",
        help="the issue, by identifier (ENG-12) or id (defaults to $LINEAR_ISSUE_ID)",
    )


def issue_ref(args):
    """The issue the script acts on: `--issue`, else `$LINEAR_ISSUE_ID`; exits 1 with neither."""
    ref = args.issue or os.environ.get(ENV_ISSUE_ID)
    if not ref:
        die(f"no issue (set {ENV_ISSUE_ID} or pass --issue)")
    return ref


def issue_name(args):
    """The issue as the output names it: `--issue`, else `$LINEAR_ISSUE`, else its id."""
    return args.issue or os.environ.get(ENV_ISSUE) or issue_ref(args)


def refuse_placeholder(text, placeholder):
    """Exit 2 when `text` is SKILL.md's placeholder, before any request."""
    if text == placeholder:
        die("refusing to send the placeholder text; substitute your own", 2)


def quoted(text):
    """`text` as Rust's `{:?}` writes a string: in double quotes, `"` and `\\` escaped, and
    each control character escaped as Rust escapes it."""
    escapes = {'"': '\\"', "\\": "\\\\", "\n": "\\n", "\r": "\\r", "\t": "\\t", "\0": "\\0"}
    out = []
    for c in text:
        if c in escapes:
            out.append(escapes[c])
        elif is_control(c):
            out.append(f"\\u{{{ord(c):x}}}")
        else:
            out.append(c)
    return '"' + "".join(out) + '"'


# --- reading a script's arguments --------------------------------------------------------

# A `T?` parameter of `update_issue` left out: it leaves the field as it is. None clears it.
KEEP = object()

# The members of the plugin's enums, in the manifest's order.
PRIORITIES = ("none", "urgent", "high", "medium", "low")
STATE_TYPES = ("triage", "backlog", "unstarted", "started", "completed", "canceled")
SORTS = (
    "manual",
    "team",
    "state",
    "priority",
    "assignee",
    "project",
    "cycle",
    "due",
    "estimate",
    "title",
    "created",
    "updated",
    "completed",
)
RELATIONS = ("blocks", "blocked_by", "related", "similar", "duplicate_of", "duplicates")


def rfc3339(text):
    """An argparse type: a time as RFC 3339 writes one, as unix milliseconds."""
    try:
        return millis(text)
    except ValueError:
        raise argparse.ArgumentTypeError(
            f"{text!r} is no RFC 3339 time (2026-11-17T09:30:00Z)"
        ) from None


def add_place_args(parser):
    """Give `parser` the two places an issue goes in its state: `--at` and `--after`."""
    parser.add_argument("--at", choices=("top", "bottom"), help="the top or the bottom")
    parser.add_argument("--after", metavar="ISSUE", help="right below this issue")


def add_file_args(parser):
    """Give `parser` `--files`, each an absolute path embedded after the text."""
    parser.add_argument(
        "--files",
        action="append",
        metavar="PATH",
        help="an absolute path, embedded after the text: an image shown, any other file linked",
    )


# The `T?` parameters of `update_issue`, which `--clear` clears.
CLEARABLE = ("estimate", "due", "assignee", "delegate", "project", "milestone", "cycle", "parent")


def add_field_args(parser):
    """Give `parser` the fields `create_issue` and `update_issue` both set, each a flag of its
    parameter's name; the description from a file with `--description_file`."""
    text = parser.add_mutually_exclusive_group()
    text.add_argument("--description", metavar="TEXT", help="markdown")
    text.add_argument("--description_file", metavar="PATH", help="the description, from a file")
    add_file_args(parser)
    parser.add_argument("--state", help="a workflow state's name, or its id")
    add_place_args(parser)
    parser.add_argument("--priority", choices=PRIORITIES)
    parser.add_argument("--estimate", type=int, metavar="N")
    parser.add_argument("--due", metavar="YYYY-MM-DD")
    parser.add_argument("--assignee", help='a display name or email, or "me"')
    parser.add_argument("--delegate", help='a display name or email, or "me"')
    parser.add_argument("--project", help="a project's name, or its id")
    parser.add_argument("--milestone", help="a milestone of the project, by name or id")
    parser.add_argument("--cycle", type=int, metavar="N", help="a cycle's number, of the team")
    parser.add_argument("--parent", metavar="ISSUE", help="an identifier (ENG-123) or a uuid")


def read_text(path):
    """The UTF-8 text of the file at `path`; exits 1 naming it when it cannot be read."""
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except (OSError, UnicodeDecodeError) as e:
        die(f"cannot read {path}: {e}")


def given(args):
    """`args` by name, each one left out dropped, and `--description_file` read into the
    description."""
    values = {name: value for name, value in vars(args).items() if value is not None}
    path = values.pop("description_file", None)
    if path is not None:
        values["description"] = read_text(path)
    return values


# --- the operations --------------------------------------------------------------------

_OPERATIONS = None


def _operations():
    """Every operation in operations.graphql, by name, as src/client.rs reads its own: each
    runs from a line starting `query ` or `mutation ` to the next one."""
    global _OPERATIONS
    if _OPERATIONS is None:
        path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "operations.graphql")
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        lines = text.split("\n")
        starts = [
            n for n, line in enumerate(lines) if line.startswith(("query ", "mutation "))
        ]
        ops = {}
        for at, start in enumerate(starts):
            end = starts[at + 1] if at + 1 < len(starts) else len(lines)
            document = "\n".join(lines[start:end]).strip()
            name = document.split(None, 1)[1].split("(", 1)[0].split("{", 1)[0].strip()
            ops[name] = document
        _OPERATIONS = ops
    return _OPERATIONS


def operation(name):
    """The document of the operation named `name`."""
    document = _operations().get(name)
    if document is None:
        raise LinearError(f"operations.graphql holds no operation {name}")
    return document


# --- HTTP ------------------------------------------------------------------------------


def _read(response, cap):
    """The body of `response`, up to `cap` bytes; past it, a LinearError."""
    if cap is None:
        return response.read()
    body = response.read(cap + 1)
    if len(body) > cap:
        raise LinearError(f"the file is over the {cap // (1024 * 1024)} MiB a file may hold")
    return body


def _send(method, url, body=None, headers=(), authorization=None, cap=None):
    """Send one request: its status, its headers and its body, whatever the status.

    The credential rides as an unredirected header, which urllib never carries onto a
    redirect: a file host that hands the download on to another host never passes it on.
    """
    request = urllib.request.Request(url, data=body, method=method)
    for name, value in headers:
        request.add_header(name, value)
    if authorization is not None:
        request.add_unredirected_header("Authorization", authorization)
    try:
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
                return response.status, response.headers, _read(response, cap)
        except urllib.error.HTTPError as e:
            with e:
                return e.code, e.headers, _read(e, cap)
    except LinearError:
        raise
    except (urllib.error.URLError, http.client.HTTPException, OSError, ValueError) as e:
        reason = getattr(e, "reason", None) or e
        raise LinearError(f"no response ({reason})") from None


def _rate_limited(stage, headers):
    """The sentence for a 429: the wait Linear asked for, when it named one."""
    wait = (headers.get("Retry-After") or "").strip() if headers else ""
    if wait.isdigit():
        return f"linear {stage}: Linear rate limited it; retry after {wait}s"
    return f"linear {stage}: Linear rate limited it"


def call(stage, name, variables):
    """Send the operation `name` with `variables` and return its `data`.

    Raises LinearError with the plugin's sentence, `linear <stage>: <why>`, for a reply that
    carries no data: Linear's own error, a rate limit, a status with no error, or an
    undecodable body.
    """
    url, authorization = need_env(ENV_API_URL, ENV_AUTHORIZATION)
    payload = json.dumps(
        {"operationName": name, "query": operation(name), "variables": variables}
    ).encode("utf-8")
    try:
        status, headers, body = _send(
            "POST",
            url,
            payload,
            [("Content-Type", "application/json")],
            authorization,
        )
    except LinearError as e:
        raise LinearError(f"linear {stage}: {e}") from None
    ok = 200 <= status < 300
    if status == 429:
        raise LinearError(_rate_limited(stage, headers))
    try:
        reply = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as e:
        if ok:
            raise undecodable(stage, e) from None
        reply = None
    errors = reply.get("errors") if isinstance(reply, dict) else None
    if isinstance(errors, list) and errors:
        first = errors[0] if isinstance(errors[0], dict) else {}
        message = first.get("message") or "an error with no message"
        if message.startswith("Entity not found"):
            raise NotFound(f"linear {stage}: {message}")
        raise LinearError(f"linear {stage}: Linear refused it ({status}): {message}")
    if not ok:
        raise LinearError(f"linear {stage}: Linear returned status {status}")
    data = reply.get("data") if isinstance(reply, dict) else None
    if not isinstance(data, dict):
        raise undecodable(stage, "the reply carries no data")
    return data


def undecodable(stage, reason):
    """The error of a reply that cannot be read as the shape asked for."""
    return LinearError(f"linear {stage}: undecodable response ({reason})")


def str_at(stage, value, key):
    """The string at `key` of `value`, or the error naming the key."""
    text = value.get(key) if isinstance(value, dict) else None
    if not isinstance(text, str):
        raise undecodable(stage, f"no string `{key}`")
    return text


def opt_str_at(value, key):
    """The string at `key` of `value`, or None when it is `null` or not there."""
    text = value.get(key) if isinstance(value, dict) else None
    return text if isinstance(text, str) else None


def is_number(value):
    """Whether `value` is a JSON number."""
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def number_at(stage, value, key):
    """The number at `key` of `value`, or the error naming the key."""
    number = value.get(key) if isinstance(value, dict) else None
    if not is_number(number):
        raise undecodable(stage, f"no number `{key}`")
    return float(number)


def nodes(stage, connection):
    """The `nodes` of the connection at `connection`, or the error saying it has none."""
    found = connection.get("nodes") if isinstance(connection, dict) else None
    if not isinstance(found, list):
        raise undecodable(stage, "a connection carries no `nodes`")
    return found


def next_cursor(connection):
    """The cursor of the page after `connection`, None on its last page."""
    info = connection.get("pageInfo") if isinstance(connection, dict) else None
    if not isinstance(info, dict) or info.get("hasNextPage") is not True:
        return None
    cursor = info.get("endCursor")
    return cursor if isinstance(cursor, str) else None


def connection_at(data, path):
    """The connection at `path` under `data`, a reply's `data`; None where a part is null."""
    for key in path:
        data = data.get(key) if isinstance(data, dict) else None
    return data


def all_nodes(stage, name, path, variables):
    """Every node of the connection at `path` that `name` reads, a page at a time from
    `after`, its other variables `variables`."""
    found, after = [], None
    while True:
        reply = call(stage, name, dict(variables, after=after))
        connection = connection_at(reply, path)
        found.extend(nodes(stage, connection))
        after = next_cursor(connection)
        if after is None:
            return found


def whole(number):
    """A whole number Linear sends as a float, rounded half away from zero as Rust's
    `f64::round` rounds it."""
    return int(math.copysign(math.floor(abs(number) + 0.5), number))


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


def opt_millis_at(stage, value, key):
    """The time at `key` of `value` in unix milliseconds, None when it is `null`."""
    text = value.get(key) if isinstance(value, dict) else None
    if text is None:
        return None
    try:
        return millis(text)
    except (ValueError, AttributeError, TypeError):
        raise undecodable(stage, f"no readable time `{key}`") from None


def millis_at(stage, value, key):
    """The time at `key` of `value` in unix milliseconds, or the error naming the key."""
    at = opt_millis_at(stage, value, key)
    if at is None:
        raise undecodable(stage, f"no time `{key}`")
    return at


def format_utc(ms):
    """`ms`, unix milliseconds, as `YYYY-MM-DDTHH:MM:SSZ` to the second: src/rfc3339.rs's
    `format_utc`, a time before 1970 written as 1970 itself."""
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(max(ms, 0) // 1000))


def is_date(text):
    """Whether `text` is a day of the calendar as `YYYY-MM-DD` writes one."""
    if len(text) != 10 or text[4] != "-" or text[7] != "-":
        return False
    parts = (text[0:4], text[5:7], text[8:10])
    if not all(part.isascii() and part.isdigit() for part in parts):
        return False
    year, month, day = (int(part) for part in parts)
    leap = year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
    days = {2: 29 if leap else 28, 4: 30, 6: 30, 9: 30, 11: 30}.get(month, 31)
    return 1 <= month <= 12 and 1 <= day <= days


def state_type(stage, kind):
    """The `linear.State_Type` Linear's `type` names, `duplicate` read as `canceled`."""
    if kind == "duplicate":
        return "canceled"
    if kind not in STATE_TYPES:
        raise undecodable(
            stage, f"a workflow state of a type Linear does not document: {quoted(kind)}"
        )
    return kind


# --- the workspace and its issues ------------------------------------------------------

_ORGANIZATION = None


def organization():
    """The workspace the credential reaches: its organization's uuid and its url key."""
    global _ORGANIZATION
    if _ORGANIZATION is None:
        stage = "read organization"
        found = call(stage, "Organization", {}).get("organization")
        _ORGANIZATION = (str_at(stage, found, "id"), str_at(stage, found, "urlKey"))
    return _ORGANIZATION


def handle(uuid, fields, shown=None):
    """A handle as the plugin's reply carries it: its `id`, `shown` or else its uuid, its key
    `linear:<organization uuid>/<uuid>`, and its fields."""
    key = f"linear:{organization()[0]}/{uuid}"
    return {"id": uuid if shown is None else shown, "key": key, "fields": fields}


def resolve_issue(ref):
    """The uuid and the identifier of the issue `ref`, its identifier or its uuid."""
    stage = "read issue"
    found = call(stage, "IssueRef", {"id": ref}).get("issue")
    return str_at(stage, found, "id"), str_at(stage, found, "identifier")


# Each connection nested in an issue `IssueList` and `IssueFull` read 20 of, and the document
# that reads the pages after it: src/client.rs's ISSUE_CONNECTIONS.
ISSUE_CONNECTIONS = (
    ("labels", "IssueLabelNames"),
    ("children", "IssueChildren"),
    ("relations", "IssueOutRelations"),
    ("inverseRelations", "IssueRelations"),
    ("attachments", "IssueAttachments"),
    ("subscribers", "IssueSubscribers"),
)


def complete(stage, node):
    """`node`, an issue read with the first page of each list nested in it, with the pages
    after it read and appended, so the issue reads as though each list came whole."""
    uuid = str_at(stage, node, "id")
    for field, document in ISSUE_CONNECTIONS:
        connection = node.get(field)
        after = next_cursor(connection)
        while after is not None:
            page = call(stage, document, {"id": uuid, "after": after})
            more = connection_at(page, ("issue", field))
            nodes(stage, connection).extend(nodes(stage, more))
            after = next_cursor(more)
    return node


def once_each(items):
    """`items`, each one already among them left out."""
    kept = []
    for item in items:
        if item not in kept:
            kept.append(item)
    return kept


def issue_fields(stage, i):
    """The `linear.Issue` handle's fields of `i`, an issue node read whole: src/client.rs's
    `parse_full_issue` and src/wire.rs's `IssueFields::of`. Its relations are read both
    ways, a related or similar issue listed once, its own relations first."""

    def names(field, key):
        return [str_at(stage, n, key) for n in nodes(stage, i.get(field))]

    priority = number_at(stage, i, "priority")
    if priority not in (0, 1, 2, 3, 4):
        raise undecodable(stage, f"a priority Linear does not have: {priority:g}")
    blocks, blocked_by, related, similar, duplicates = [], [], [], [], []
    duplicate_of = None
    for relation in nodes(stage, i.get("relations")):
        other = str_at(stage, relation.get("relatedIssue"), "identifier")
        kind = relation.get("type")
        if kind == "blocks":
            blocks.append(other)
        elif kind == "related":
            related.append(other)
        elif kind == "similar":
            similar.append(other)
        elif kind == "duplicate" and duplicate_of is None:
            duplicate_of = other
    for relation in nodes(stage, i.get("inverseRelations")):
        other = str_at(stage, relation.get("issue"), "identifier")
        kind = relation.get("type")
        if kind == "blocks":
            blocked_by.append(other)
        elif kind == "related":
            related.append(other)
        elif kind == "similar":
            similar.append(other)
        elif kind == "duplicate":
            duplicates.append(other)
    estimate = i.get("estimate")
    cycle = (i.get("cycle") or {}).get("number")
    state = i.get("state")
    return {
        "uuid": str_at(stage, i, "id"),
        "workspace": organization()[1],
        "team": str_at(stage, i.get("team"), "key"),
        "title": str_at(stage, i, "title"),
        "url": str_at(stage, i, "url"),
        "description": opt_str_at(i, "description") or "",
        "branch": str_at(stage, i, "branchName"),
        "state": str_at(stage, state, "name"),
        "state_type": state_type(stage, str_at(stage, state, "type")),
        "priority": PRIORITIES[int(priority)],
        "estimate": whole(estimate) if is_number(estimate) else None,
        "due": opt_str_at(i, "dueDate"),
        "assignee": opt_str_at(i.get("assignee"), "displayName"),
        "delegate": opt_str_at(i.get("delegate"), "displayName"),
        "creator": opt_str_at(i.get("creator"), "displayName"),
        "project": opt_str_at(i.get("project"), "name"),
        "milestone": opt_str_at(i.get("projectMilestone"), "name"),
        "cycle": whole(cycle) if is_number(cycle) else None,
        "labels": names("labels", "name"),
        "parent": opt_str_at(i.get("parent"), "identifier"),
        "sub_issues": names("children", "identifier"),
        "blocks": blocks,
        "blocked_by": blocked_by,
        "related": once_each(related),
        "similar": once_each(similar),
        "duplicates": duplicates,
        "duplicate_of": duplicate_of,
        "links": names("attachments", "url"),
        "subscribers": names("subscribers", "displayName"),
        "created_at": millis_at(stage, i, "createdAt"),
        "updated_at": millis_at(stage, i, "updatedAt"),
        "completed_at": opt_millis_at(stage, i, "completedAt"),
        "canceled_at": opt_millis_at(stage, i, "canceledAt"),
        "archived": opt_millis_at(stage, i, "archivedAt") is not None,
        "trashed": i.get("trashed") is True,
    }


def issue_handle(stage, node):
    """The `linear.Issue` handle of `node`, an issue node, its nested lists read whole."""
    fields = issue_fields(stage, complete(stage, node))
    return handle(fields["uuid"], fields, str_at(stage, node, "identifier"))


def issue(ref):
    """The handle of the issue `ref` names, by its identifier or its uuid, read whole."""
    stage = "read issue"
    return issue_handle(stage, call(stage, "IssueFull", {"id": ref}).get("issue"))


def read_issue(ref):
    """`linear.issue`: the handle of the issue `ref` names, an issue Linear does not hold
    refused naming it and the workspace, as the plugin refuses it."""
    workspace = organization()[1]
    try:
        return issue(ref)
    except NotFound as e:
        raise LinearError(f"issue: no issue {ref} in workspace {workspace}: {e}") from None


# --- `issues`: ports of src/reads.rs -----------------------------------------------------

# The time filters of `issues`, each with the field of Linear's it compares and how.
TIME_FILTERS = (
    ("created_after", "createdAt", "gt"),
    ("created_before", "createdAt", "lt"),
    ("updated_after", "updatedAt", "gt"),
    ("updated_before", "updatedAt", "lt"),
    ("completed_after", "completedAt", "gt"),
    ("completed_before", "completedAt", "lt"),
    ("canceled_after", "canceledAt", "gt"),
    ("canceled_before", "canceledAt", "lt"),
)

# The due-date filters of `issues`, each with how it compares the due date.
DUE_FILTERS = (("due_after", "gt"), ("due_before", "lt"))

# The filters of `issues` that take a list, in the manifest's order.
LIST_FILTERS = (
    "id",
    "team",
    "state",
    "state_type",
    "priority",
    "labels",
    "without_labels",
    "assignee",
    "delegate",
    "project",
    "milestone",
    "cycle",
    "parent",
)

# The element of Linear's `sort` each `order` key is: `manual` top to bottom, a date newest
# first, and every other key as Linear orders it.
SORT_INPUTS = {
    "manual": {"manual": {"order": "Ascending"}},
    "team": {"team": {"nulls": "last"}},
    "state": {"workflowState": {"nulls": "last"}},
    "priority": {"priority": {"nulls": "last"}},
    "assignee": {"assignee": {"nulls": "last"}},
    "project": {"project": {"nulls": "last"}},
    "cycle": {"cycle": {"nulls": "last"}},
    "due": {"dueDate": {"order": "Descending"}},
    "estimate": {"estimate": {"nulls": "last"}},
    "title": {"title": {"nulls": "last"}},
    "created": {"createdAt": {"order": "Descending"}},
    "updated": {"updatedAt": {"order": "Descending"}},
    "completed": {"completedAt": {"order": "Descending"}},
}


def is_uuid(text):
    """Whether `text` is shaped as a uuid: 8-4-4-4-12 hexadecimal digits."""
    return len(text) == 36 and all(
        c == "-" if at in (8, 13, 18, 23) else c in string.hexdigits for at, c in enumerate(text)
    )


def refs_filter(param, refs):
    """The filter keeping the issues `refs`, identifiers or uuids, name: the uuids by Linear's
    id, and each identifier by its team's key and its number."""
    uuids, any_of = [ref for ref in refs if is_uuid(ref)], []
    if uuids:
        any_of.append({"id": {"in": uuids}})
    for ref in refs:
        if is_uuid(ref):
            continue
        team, dash, number = ref.rpartition("-")
        if not (dash and team and number.isascii() and number.isdigit()):
            raise LinearError(
                f"issues: {param} {quoted(ref)} is neither an identifier (ENG-123) nor a uuid"
            )
        any_of.append({"team": {"key": {"eq": team}}, "number": {"eq": int(number)}})
    return {"or": any_of}


def member_filter(who):
    """The user filter keeping an issue held by any of `who`: `me` whoever the credential
    acts as, `none` nobody, and any other text a user by display name or email."""
    any_of = []
    for name in who:
        if name == "me":
            any_of.append({"isMe": {"eq": True}})
        elif name == "none":
            any_of.append({"null": True})
        else:
            any_of.extend([{"displayName": {"eq": name}}, {"email": {"eq": name}}])
    return {"or": any_of}


def issue_filter(q):
    """The `IssueFilter` keeping what `q`, the `issues` arguments by name, keeps: one clause
    per filter given, all of them holding, `{}` for none. src/reads.rs's `issue_filter`."""
    for param in LIST_FILTERS:
        if q.get(param) == []:
            raise LinearError(f"issues: `{param}` is empty; leave it out to keep every issue")
    every = []
    if q.get("id"):
        every.append(refs_filter("id", q["id"]))
    if q.get("team"):
        every.append({"team": {"key": {"in": q["team"]}}})
    if q.get("state"):
        every.append({"state": {"name": {"in": q["state"]}}})
    if q.get("state_type"):
        kinds = []
        for kind in q["state_type"]:
            kinds.extend((kind, "duplicate") if kind == "canceled" else (kind,))
        every.append({"state": {"type": {"in": kinds}}})
    if q.get("priority"):
        every.append({"priority": {"in": [PRIORITIES.index(p) for p in q["priority"]]}})
    for label in q.get("labels") or []:
        every.append({"labels": {"some": {"name": {"eq": label}}}})
    if q.get("without_labels"):
        every.append({"labels": {"every": {"name": {"nin": q["without_labels"]}}}})
    if q.get("assignee"):
        every.append({"assignee": member_filter(q["assignee"])})
    if q.get("delegate"):
        every.append({"delegate": member_filter(q["delegate"])})
    if q.get("project"):
        every.append({"project": {"name": {"in": q["project"]}}})
    if q.get("milestone"):
        every.append({"projectMilestone": {"name": {"in": q["milestone"]}}})
    if q.get("cycle"):
        every.append({"cycle": {"number": {"in": q["cycle"]}}})
    if q.get("parent"):
        every.append({"parent": refs_filter("parent", q["parent"])})
    if q.get("blocked") is not None:
        every.append({"hasBlockedByRelations": {"eq": q["blocked"]}})
    if q.get("text") is not None:
        text = q["text"]
        every.append(
            {
                "or": [
                    {"title": {"containsIgnoreCase": text}},
                    {"description": {"containsIgnoreCase": text}},
                ]
            }
        )
    for param, field, compare in TIME_FILTERS:
        if q.get(param) is not None:
            every.append({field: {compare: format_utc(q[param])}})
    for param, compare in DUE_FILTERS:
        day = q.get(param)
        if day is not None:
            if not is_date(day):
                raise LinearError(f"issues: {param} {quoted(day)} is no YYYY-MM-DD date")
            every.append({"dueDate": {compare: day}})
    return {"and": every} if every else {}


def issues(q):
    """The handles of the issues `q`, the `issues` arguments by name, keeps, each read whole,
    in the order its `order` gives, at most its `limit`: 20 to a page, as src/client.rs's
    `issue_list` reads them."""
    limit = q.get("limit", 50)
    if not 1 <= limit <= 250:
        raise LinearError(f"issues: limit {limit} is outside 1 to 250")
    order = q.get("order") or ["team", "state", "manual"]
    if len(order) > 3:
        raise LinearError(f"issues: `order` names {len(order)} keys, and Linear sorts by at most 3")
    variables = {
        "filter": issue_filter(q),
        "sort": [SORT_INPUTS[key] for key in order],
        "includeArchived": bool(q.get("include_archived")),
    }
    stage = "read issues"
    found, after = [], None
    while len(found) < limit:
        variables.update(first=min(limit - len(found), 20), after=after)
        connection = call(stage, "IssueList", variables).get("issues")
        found.extend(issue_handle(stage, node) for node in nodes(stage, connection))
        after = next_cursor(connection)
        if after is None:
            break
    return found[:limit]


# --- comments ----------------------------------------------------------------------------


def comment_fields(stage, c, on):
    """The `linear.Comment` handle's fields of `c`, a comment node on the issue whose
    identifier is `on`: src/client.rs's `parse_thread_comment`."""
    author = (
        opt_str_at(c.get("user"), "displayName")
        or opt_str_at(c.get("botActor"), "name")
        or opt_str_at(c.get("externalUser"), "name")
    )
    return {
        "issue": on,
        "body": str_at(stage, c, "body"),
        "url": str_at(stage, c, "url"),
        "author": author,
        "reply_to": opt_str_at(c.get("parent"), "id"),
        "resolved_at": opt_millis_at(stage, c, "resolvedAt"),
        "created_at": millis_at(stage, c, "createdAt"),
        "edited_at": opt_millis_at(stage, c, "editedAt"),
    }


def threaded(found):
    """`found`, `(uuid, fields)` of each comment, in the order a thread reads: the comments
    that answer none oldest first, ties broken by id, each followed at once by its replies in
    the same order, depth first; replies that answer one another in a ring go last.
    src/reads.rs's `threaded`."""
    found = sorted(found, key=lambda c: (c[1]["created_at"], c[0]))
    held = {uuid for uuid, _ in found}
    replies, roots = {}, []
    for at, (_, fields) in enumerate(found):
        if fields["reply_to"] in held:
            replies.setdefault(fields["reply_to"], []).append(at)
        else:
            roots.append(at)
    order, placed, stack = [], [False] * len(found), roots[::-1]
    while stack:
        at = stack.pop()
        if placed[at]:
            continue
        placed[at] = True
        order.append(at)
        stack.extend(replies.get(found[at][0], [])[::-1])
    order.extend(at for at in range(len(found)) if not placed[at])
    return [found[at] for at in order]


def comments(ref):
    """The handles of the comments on the issue `ref`, in the order its thread reads."""
    uuid = resolve_issue(ref)[0]
    stage = "read comments"
    found, on, after = [], None, None
    while True:
        node = call(stage, "IssueThread", {"id": uuid, "after": after}).get("issue")
        on = on or str_at(stage, node, "identifier")
        connection = node.get("comments")
        for c in nodes(stage, connection):
            found.append((str_at(stage, c, "id"), comment_fields(stage, c, on)))
        after = next_cursor(connection)
        if after is None:
            break
    return [handle(uuid, fields) for uuid, fields in threaded(found)]


def comment(uuid):
    """The comment `uuid`, read as `comments` reads it: the uuid of the issue it is on, and
    its handle."""
    stage = "read comment"
    node = call(stage, "CommentOne", {"id": uuid}).get("comment")
    on = node.get("issue") if isinstance(node, dict) else None
    fields = comment_fields(stage, node, str_at(stage, on, "identifier"))
    return str_at(stage, on, "id"), handle(str_at(stage, node, "id"), fields)


# --- teams, states and labels, and names turned into ids: ports of src/lookup.rs ---------


def team_by_key(key):
    """The id of the team whose key is `key`: src/client.rs's `resolve_team`."""
    stage = "resolve team"
    found = call(stage, "TeamByKey", {"filter": {"key": {"eq": key}}}).get("teams")
    for team in nodes(stage, found):
        if str_at(stage, team, "key") == key:
            return str_at(stage, team, "id")
    raise LinearError(f"linear {stage}: no team with the key '{key}'")


def states_of(team_id):
    """The workflow states of the team `team_id`, in its workflow's order: each one's id,
    name and `linear.State_Type`."""
    stage = "read states"
    found = connection_at(call(stage, "States", {"teamId": team_id}), ("team", "states"))
    rows = [
        (
            s.get("position") if is_number(s.get("position")) else 0,
            str_at(stage, s, "id"),
            str_at(stage, s, "name"),
            state_type(stage, str_at(stage, s, "type")),
        )
        for s in nodes(stage, found)
    ]
    return [row[1:] for row in sorted(rows, key=lambda row: row[0])]


def states(team):
    """The handles of the workflow states of the team whose key is `team`."""
    return [
        handle(uuid, {"name": name, "type": kind, "team": team})
        for uuid, name, kind in states_of(team_by_key(team))
    ]


def label_rows(team=None):
    """The workspace's labels and, with `team`, that team's, by key: each one's id and its
    handle's fields."""
    stage = "read labels"
    workspace = {"team": {"null": True}}
    scope = {"or": [workspace, {"team": {"key": {"eq": team}}}]} if team else workspace
    return [
        (
            str_at(stage, label, "id"),
            {
                "name": str_at(stage, label, "name"),
                "group": opt_str_at(label.get("parent"), "name"),
                "team": opt_str_at(label.get("team"), "key"),
            },
        )
        for label in all_nodes(stage, "LabelList", ("issueLabels",), {"filter": scope})
    ]


def labels(team=None):
    """The handles of the workspace's labels and, with `team`, that team's, by key."""
    return [handle(uuid, fields) for uuid, fields in label_rows(team)]


def there_are(names):
    """What a refusal says there is: `there are: a, b`, or `there are none`."""
    return f"there are: {', '.join(names)}" if names else "there are none"


def pick(action, what, scope, name, candidates):
    """The one of `candidates`, each `(id, name, alias)`, that `name` names: the one whose id
    it is, else the one answering to it, refused when none or several do, `scope` saying
    where (` in team ENG`). src/lookup.rs's `pick`."""
    for candidate in candidates:
        if candidate[0] == name:
            return candidate
    named = [c for c in candidates if name in (c[1], c[2])]
    if len(named) == 1:
        return named[0]
    if not named:
        there = there_are([c[1] for c in candidates])
        raise LinearError(f"{action}: no {what} named {quoted(name)}{scope}; {there}")
    ids = ", ".join(c[0] for c in named)
    raise LinearError(
        f"{action}: {len(named)} {what}s are named {quoted(name)}{scope}: {ids}; pass the id "
        "of the one you mean in place of the name"
    )


def lookup_team(action, key):
    """The team whose key, or id, is `key`, as `(id, key)`."""
    stage = "read teams"
    candidates = [
        (str_at(stage, team, "id"), str_at(stage, team, "key"), None)
        for team in all_nodes(stage, "TeamList", ("teams",), {})
    ]
    return pick(action, "team", "", key, candidates)[:2]


def lookup_state(action, team, name):
    """The state of `team`, `(id, key)`, named `name`, or whose id it is, as `(id, name)`."""
    candidates = [(uuid, state, None) for uuid, state, _ in states_of(team[0])]
    return pick(action, "state", f" in team {team[1]}", name, candidates)[:2]


def lookup_label(action, team, name):
    """The id of the label named `name`, or whose id it is, for an issue of `team`, `(id,
    key)`: the team's label of that name, else the workspace's. src/lookup.rs's `label`."""
    rows = label_rows(team[1])
    of_team = [(uuid, f["name"], None) for uuid, f in rows if f["team"] is not None]
    of_workspace = [(uuid, f["name"], None) for uuid, f in rows if f["team"] is None]
    for candidate in of_team + of_workspace:
        if candidate[0] == name:
            return candidate[0]
    scopes = ((of_team, f" in team {team[1]}"), (of_workspace, " in the workspace"))
    for candidates, scope in scopes:
        if any(c[1] == name for c in candidates):
            return pick(action, "label", scope, name, candidates)[0]
    there = there_are([c[1] for c in of_team + of_workspace])
    raise LinearError(
        f"{action}: no label named {quoted(name)} in team {team[1]} or the workspace; {there}"
    )


def lookup_user(action, name):
    """The id of the user `name` names: `me` whoever the credential acts as, else the user
    whose display name or email, or id, it is."""
    if name == "me":
        stage = "read viewer"
        return str_at(stage, call(stage, "Viewer", {}).get("viewer"), "id")
    stage = "read users"
    candidates = [
        (str_at(stage, u, "id"), str_at(stage, u, "displayName"), str_at(stage, u, "email"))
        for u in all_nodes(stage, "UserList", ("users",), {})
    ]
    return pick(action, "user", "", name, candidates)[0]


def lookup_project(action, name):
    """The project named `name`, or whose id it is, as `(id, name)`."""
    stage = "read projects"
    candidates = [
        (str_at(stage, p, "id"), str_at(stage, p, "name"), None)
        for p in all_nodes(stage, "ProjectList", ("projects",), {"filter": {}})
    ]
    return pick(action, "project", "", name, candidates)[:2]


def lookup_milestone(action, project, name):
    """The id of the milestone of `project`, `(id, name)`, named `name` or whose id it is."""
    stage = "read milestones"
    path = ("project", "projectMilestones")
    rows = sorted(
        (
            (number_at(stage, m, "sortOrder"), str_at(stage, m, "id"), str_at(stage, m, "name"))
            for m in all_nodes(stage, "MilestoneList", path, {"id": project[0]})
        ),
        key=lambda row: row[0],
    )
    candidates = [(uuid, milestone, None) for _, uuid, milestone in rows]
    return pick(action, "milestone", f" in project {project[1]}", name, candidates)[0]


def lookup_cycle(action, team, number):
    """The id of the cycle of `team`, `(id, key)`, numbered `number`."""
    stage = "read cycles"
    scope = {"filter": {"team": {"id": {"eq": team[0]}}}}
    rows = sorted(
        (whole(number_at(stage, c, "number")), str_at(stage, c, "id"))
        for c in all_nodes(stage, "CycleList", ("cycles",), scope)
    )
    for n, uuid in rows:
        if n == number:
            return uuid
    there = there_are([str(n) for n, _ in rows])
    raise LinearError(f"{action}: no cycle {number} in team {team[1]}; {there}")


# --- the writes: ports of src/writes.rs ---------------------------------------------------

# How far apart `at` places an issue from the one at the top or the bottom of its state.
GAP = 1000.0

# Each relation as Linear's relation it is, and whether the issue holds it: True when the
# issue is the relation's `issueId` and the other its `relatedIssueId`.
RELATION_KINDS = {
    "blocks": ("blocks", True),
    "blocked_by": ("blocks", False),
    "related": ("related", True),
    "similar": ("similar", True),
    "duplicate_of": ("duplicate", True),
    "duplicates": ("duplicate", False),
}


def place_of(uuid):
    """Where the issue `uuid` sits: its identifier, its team `(id, key)`, its state `(id,
    name)`, and its project's id, None when it is in none."""
    stage = "read issue"
    i = call(stage, "IssuePlace", {"id": uuid}).get("issue")
    team, state = (i or {}).get("team"), (i or {}).get("state")
    return {
        "identifier": str_at(stage, i, "identifier"),
        "team": (str_at(stage, team, "id"), str_at(stage, team, "key")),
        "state": (str_at(stage, state, "id"), str_at(stage, state, "name")),
        "project": opt_str_at(i.get("project"), "id"),
    }


def in_state(team, state_id, leave_out=None):
    """The uuids and sort orders of the issues in the state `state_id` of `team`, `(id,
    key)`, in manual order, the issue `leave_out` left out."""
    stage = "read issues"
    variables = {
        "filter": {"team": {"id": {"eq": team[0]}}, "state": {"id": {"eq": state_id}}},
        "sort": [{"manual": {"order": "Ascending"}}],
        "first": 25,
    }
    rows = [
        (str_at(stage, i, "id"), number_at(stage, i, "sortOrder"))
        for i in all_nodes(stage, "Issues", ("issues",), variables)
    ]
    return [row for row in rows if row[0] != leave_out]


def sort_order_for(spot, others):
    """The `sortOrder` that puts an issue at `spot`, `top`, `bottom` or the uuid of the issue
    it goes right below, among `others`: above the first or below the last by GAP, or halfway
    between the issue it goes after and the next, else below it by the gap. None when there
    is nothing to place it against."""
    if spot == "top":
        return others[0][1] - GAP if others else None
    if spot == "bottom":
        return others[-1][1] + GAP if others else None
    at = next((n for n, (uuid, _) in enumerate(others) if uuid == spot), None)
    if at is None:
        return None
    if at + 1 < len(others):
        return (others[at][1] + others[at + 1][1]) / 2
    return others[at][1] + GAP


def placed_after(action, after, team, state):
    """Refuse an issue `after` to place after that is not in the state `state`, `(id,
    name)`, of `team`, `(id, key)`."""
    other = place_of(after)
    if other["state"][0] != state[0]:
        raise LinearError(
            f"{action}: after {other['identifier']} is in {other['state'][1]} of "
            f"{other['team'][1]}, not {state[1]} of {team[1]}"
        )


def spot_of(args):
    """Where `args`' `at` or `after` places the issue: `top`, `bottom`, the uuid of the issue
    it goes right below, or None when neither is given."""
    if args.get("after") is not None:
        return resolve_issue(args["after"])[0]
    return args.get("at")


def sort_order(action, spot, team, state, moving=None):
    """The `sortOrder` that puts an issue at `spot` in the state `state`, `(id, name)`, of
    `team`, `(id, key)`, the issue `moving`, `(uuid, identifier)`, left out of the others:
    KEEP for no spot or nothing to place it against. An issue it goes after is checked to be
    in that state, and not to be the issue itself."""
    if spot is None:
        return KEEP
    uuid = moving[0] if moving else None
    if spot not in ("top", "bottom"):
        if spot == uuid:
            raise LinearError(f"{action}: after is {moving[1]} itself")
        placed_after(action, spot, team, state)
    order = sort_order_for(spot, in_state(team, state[0], uuid))
    return KEEP if order is None else order


def read_upload(action, path):
    """The file at `path` to put up, as `(name, content_type, bytes)`: src/file.rs's
    `read_upload`, refusing a relative path, one that names no file, a path with nothing
    there or no regular file, one that cannot be read, and a file over MAX_FILE_BYTES."""
    if not os.path.isabs(path):
        raise LinearError(f"{action}: files must be absolute, not {quoted(path)}")
    name = os.path.basename(path)
    if not name or name in (".", ".."):
        raise LinearError(f"{action}: {path} names no file")
    try:
        with open(path, "rb") as handle:
            if not os.path.isfile(path):
                raise LinearError(f"{action}: {path} is not a regular file")
            data = handle.read(MAX_FILE_BYTES + 1)
    except FileNotFoundError:
        raise LinearError(f"{action}: no file at {path}") from None
    except IsADirectoryError:
        raise LinearError(f"{action}: {path} is not a regular file") from None
    except OSError as e:
        raise LinearError(f"{action}: cannot read {path}: {e}") from None
    if len(data) > MAX_FILE_BYTES:
        mib = MAX_FILE_BYTES // (1024 * 1024)
        raise LinearError(f"{action}: {path} is over the {mib} MiB a file may hold")
    return name, content_type_of(name), data


def embedded(text, uploads):
    """Put each of `uploads` up, and return the markdown `text` is sent as with every file in
    it."""
    return embed(text, [(name, kind, upload(name, kind, data)) for name, kind, data in uploads])


def issue_input(fields):
    """The `IssueUpdateInput` of `fields`, Linear's keys: one KEEP left out, and one None
    sent as `null`, which clears the field."""
    return {key: value for key, value in fields.items() if value is not KEEP}


def create_issue(new):
    """File the issue `new`, the `create_issue` arguments by name, and give it back: every
    name looked up, and the issue it goes after checked, before a file goes up or the issue
    is filed; placed with no `state`, it is placed in the team's default state.
    src/writes.rs's `create_issue`."""
    action = "create_issue"
    if new.get("at") is not None and new.get("after") is not None:
        raise LinearError(f"{action}: at and after cannot both be given")
    due = new.get("due")
    if due is not None and not is_date(due):
        raise LinearError(f"{action}: due {quoted(due)} is no YYYY-MM-DD date")
    if new.get("milestone") is not None and new.get("project") is None:
        raise LinearError(
            f"{action}: milestone {quoted(new['milestone'])} needs a project to be in, and none "
            "is given"
        )
    uploads = [read_upload(action, path) for path in new.get("files") or []]
    spot = spot_of(new)
    parent = resolve_issue(new["parent"])[0] if new.get("parent") is not None else KEEP
    team = lookup_team(action, new["team"])
    state = None
    if new.get("state") is not None:
        state = lookup_state(action, team, new["state"])
    elif spot is not None:
        stage = "read default state"
        found = call(stage, "TeamDefaultState", {"id": team[0]})
        default = connection_at(found, ("team", "defaultIssueState"))
        if default is None:
            raise LinearError(
                f"{action}: team {team[1]} has no default state to place the issue in; name its "
                "state"
            )
        state = (str_at(stage, default, "id"), str_at(stage, default, "name"))
    label_ids = []
    for name in new.get("labels") or []:
        label = lookup_label(action, team, name)
        if label not in label_ids:
            label_ids.append(label)

    def user(name):
        return KEEP if name is None else lookup_user(action, name)

    project = None
    if new.get("project") is not None:
        project = lookup_project(action, new["project"])
    milestone = KEEP
    if new.get("milestone") is not None:
        milestone = lookup_milestone(action, project, new["milestone"])
    cycle = KEEP
    if new.get("cycle") is not None:
        cycle = lookup_cycle(action, team, new["cycle"])
    fields = {
        "teamId": team[0],
        "stateId": KEEP if state is None else state[0],
        "sortOrder": KEEP,
        "title": new["title"],
        "description": KEEP,
        "priority": KEEP if new.get("priority") is None else PRIORITIES.index(new["priority"]),
        "estimate": KEEP if new.get("estimate") is None else new["estimate"],
        "dueDate": KEEP if due is None else due,
        "assigneeId": user(new.get("assignee")),
        "delegateId": user(new.get("delegate")),
        "projectId": KEEP if project is None else project[0],
        "projectMilestoneId": milestone,
        "cycleId": cycle,
        "parentId": parent,
    }
    if state is not None:
        fields["sortOrder"] = sort_order(action, spot, team, state)
    if new.get("description") is not None or uploads:
        fields["description"] = embedded(new.get("description") or "", uploads)
    created = issue_input(fields)
    if label_ids:
        created["labelIds"] = label_ids
    stage = "create issue"
    made = connection_at(call(stage, "IssueCreate", {"input": created}), ("issueCreate", "issue"))
    return issue(str_at(stage, made, "id"))


def update_issue(change):
    """Change the issue as `change`, the `update_issue` arguments by name, asks, and give it
    back: every name looked up before a file goes up or anything changes. A `T?` argument
    KEEP leaves its field as it is, and None clears it. src/writes.rs's `update_issue`."""
    action = "update_issue"
    files = change.get("files") or []
    if change.get("team") is not None and change.get("state") is None:
        team = change["team"]
        raise LinearError(f"{action}: team {team} needs a state of team {team} beside it")
    if files and change.get("description") is None:
        raise LinearError(f"{action}: files go only beside description")
    if change.get("at") is not None and change.get("after") is not None:
        raise LinearError(f"{action}: at and after cannot both be given")
    due = change.get("due", KEEP)
    if isinstance(due, str) and not is_date(due):
        raise LinearError(f"{action}: due {quoted(due)} is no YYYY-MM-DD date")
    project = change.get("project", KEEP)
    milestone = change.get("milestone", KEEP)
    if isinstance(milestone, str) and project is None:
        raise LinearError(
            f"{action}: milestone {quoted(milestone)} needs a project to be in, and project is "
            "nil"
        )
    uploads = [read_upload(action, path) for path in files]
    uuid = resolve_issue(change["issue"])[0]
    spot = spot_of(change)
    parent = change.get("parent", KEEP)
    if isinstance(parent, str):
        parent = resolve_issue(parent)[0]
    where = place_of(uuid)
    team = where["team"]
    if change.get("team") is not None:
        team = lookup_team(action, change["team"])
    state = where["state"]
    if change.get("state") is not None:
        state = lookup_state(action, team, change["state"])
    if isinstance(project, str):
        project = lookup_project(action, project)
    if isinstance(milestone, str):
        if isinstance(project, tuple):
            in_project = project
        elif project is KEEP and where["project"] is not None:
            in_project = lookup_project(action, where["project"])
        else:
            raise LinearError(
                f"{action}: milestone {quoted(milestone)} needs a project to be in, and "
                f"{where['identifier']} is in none"
            )
        milestone = lookup_milestone(action, in_project, milestone)

    def user(name):
        return lookup_user(action, name) if isinstance(name, str) else name

    priority = change.get("priority")
    cycle = change.get("cycle", KEEP)
    if isinstance(cycle, int):
        cycle = lookup_cycle(action, team, cycle)
    fields = {
        "teamId": KEEP if change.get("team") is None else team[0],
        "stateId": KEEP if change.get("state") is None else state[0],
        "sortOrder": sort_order(action, spot, team, state, (uuid, where["identifier"])),
        "title": KEEP if change.get("title") is None else change["title"],
        "description": KEEP,
        "priority": KEEP if priority is None else PRIORITIES.index(priority),
        "estimate": change.get("estimate", KEEP),
        "dueDate": due,
        "assigneeId": user(change.get("assignee", KEEP)),
        "delegateId": user(change.get("delegate", KEEP)),
        "projectId": project[0] if isinstance(project, tuple) else project,
        "projectMilestoneId": milestone,
        "cycleId": cycle,
        "parentId": parent,
    }
    if change.get("description") is not None:
        fields["description"] = embedded(change["description"], uploads)
    update = issue_input(fields)
    if update:
        call("update issue", "IssueWrite", {"id": uuid, "input": update})
    return issue(uuid)


def change_label(ref, label, add):
    """Put the label `label` on the issue `ref`, or with `add` false take it off, and give the
    issue back: a label it already carries, or does not, is left as it is."""
    action = "add_label" if add else "remove_label"
    uuid = resolve_issue(ref)[0]
    label_id = lookup_label(action, place_of(uuid)["team"], label)
    stage = "read labels"
    on = connection_at(call(stage, "IssueLabelIds", {"id": uuid}), ("issue", "labels"))
    carried = label_id in [str_at(stage, l, "id") for l in nodes(stage, on)]
    # Linear refuses to take off a label the issue does not carry.
    if add != carried:
        name = "IssueAddLabel" if add else "IssueRemoveLabel"
        call(action.replace("_", " "), name, {"id": uuid, "labelId": label_id})
    return issue(uuid)


def relations(uuid, held):
    """The relations of the issue `uuid`, the ones it holds when `held`, else the ones that
    name it: each one's id, Linear's relation, and the id of the issue at its other end."""
    stage = "read relations"
    name, field, other = (
        ("RelationsOut", "relations", "relatedIssue")
        if held
        else ("RelationsIn", "inverseRelations", "issue")
    )
    return [
        (str_at(stage, r, "id"), str_at(stage, r, "type"), str_at(stage, r.get(other), "id"))
        for r in all_nodes(stage, name, ("issue", field), {"id": uuid})
    ]


def add_relation(ref, relation, other_ref):
    """Relate the issue `ref` to `other_ref` as `relation`, unless it already is, and give the
    issue back: src/writes.rs's `change_issue` for `add_relation`."""
    action = "add_relation"
    uuid, identifier = resolve_issue(ref)
    other = resolve_issue(other_ref)[0]
    if other == uuid:
        raise LinearError(f"{action}: other is {identifier} itself")
    kind, holds = RELATION_KINDS[relation]
    symmetric = relation in ("related", "similar")
    held = [
        r
        for side in (True, False)
        if side == holds or symmetric
        for r in relations(uuid, side)
        if r[1] == kind and r[2] == other
    ]
    if not held:
        source, target = (uuid, other) if holds else (other, uuid)
        related = {"issueId": source, "relatedIssueId": target, "type": kind}
        call("relate issues", "RelationCreate", {"input": related})
    return issue(uuid)


def archive_issue(ref):
    """Archive the issue `ref`, its sub-issues with it, and give it back."""
    uuid = resolve_issue(ref)[0]
    call("archive issue", "IssueArchive", {"id": uuid})
    return issue(uuid)


def create_comment(ref, body, files=(), reply_to=None):
    """Post `body` as a comment on the issue `ref`, each of `files` embedded after it, in the
    thread of the comment `reply_to` when one is given, and give the comment back: the issue
    and the comment it answers checked before a file goes up, and a reply to a reply put in
    the thread both are in. src/writes.rs's `create_comment`."""
    action = "create_comment"
    uploads = [read_upload(action, path) for path in files]
    uuid, identifier = resolve_issue(ref)
    post = {"issueId": uuid}
    if reply_to is not None:
        on, answered = comment(reply_to)
        if on != uuid:
            raise LinearError(
                f"{action}: reply_to {reply_to} is on {answered['fields']['issue']}, not "
                f"{identifier}"
            )
        post["parentId"] = answered["fields"]["reply_to"] or answered["id"]
    post["body"] = embedded(body, uploads)
    stage = "post comment"
    posted = call(stage, "CommentPost", {"input": post})
    posted = connection_at(posted, ("commentCreate", "comment"))
    return comment(str_at(stage, posted, "id"))[1]


def description(uuid):
    """The issue's description, "" for none."""
    data = call("read description", "IssueDescription", {"id": uuid})
    return (data.get("issue") or {}).get("description") or ""


def bodies(uuid):
    """The bodies of the issue `uuid`'s comments, oldest first as `(createdAt, id)` orders
    them, afkd's control markers left out. Linear writes every time in UTC to the
    millisecond, so the text sorts as the time does."""
    stage = "read comments"
    found = all_nodes(stage, "IssueComments", ("issue", "comments"), {"id": uuid})
    found.sort(key=lambda c: (c.get("createdAt") or "", c.get("id") or ""))
    return [c.get("body") or "" for c in found if not is_control_marker(c.get("body") or "")]


def files(ref):
    """The urls of every file Linear hosts that the issue `ref` links: in its description,
    then in its thread oldest first, control markers left out, each url once. file.rs's
    `linked_issue_files`."""
    uuid = resolve_issue(ref)[0]
    found = []
    for text in [description(uuid)] + bodies(uuid):
        for _, url in linked_files(text, is_linear_file):
            if url not in found:
                found.append(url)
    return found


# --- the thread ------------------------------------------------------------------------

# What every control marker opens with (src/issue.rs's AFKD_MARK): the plugin's own
# bookkeeping and the notes a config keeps under a mark of its own alike. A comment opening
# with it is afkd's, which `files` reads no file out of.
AFKD_MARK = "[afkd-"


def unescape(text):
    """`text` with CommonMark's backslash escapes taken out: a backslash before an ASCII
    punctuation character dropped, every other character kept. src/claim.rs's `unescape`."""
    out = []
    n = 0
    while n < len(text):
        c = text[n]
        if c == "\\" and n + 1 < len(text) and text[n + 1] in string.punctuation:
            out.append(text[n + 1])
            n += 2
            continue
        out.append(c)
        n += 1
    return "".join(out)


def is_control_marker(text):
    """Whether a comment is one of afkd's control markers, escaped or as written."""
    return rust_trim_start(unescape(text)).startswith(AFKD_MARK)


# --- files: ports of src/file.rs ---------------------------------------------------------

# The most one file may hold, either way: file.rs's MAX_FILE_BYTES.
MAX_FILE_BYTES = 10 * 1024 * 1024

# The bounds file.rs's `linked_files` reads a link within, each in UTF-8 bytes.
MAX_LINK_TEXT = 1024
MAX_LINK_DESTINATION = 2048
MAX_LINK_PARENS = 32

# Where Linear keeps the files put on its issues: client.rs's UPLOADS_BASE.
UPLOADS_BASE = "https://uploads.linear.app"

# Rust's `char::is_whitespace`, the Unicode White_Space property. Python's `str.isspace`
# also takes U+001C to U+001F, which Rust does not.
RUST_WHITESPACE = frozenset(
    "\t\n\x0b\x0c\r \x85\xa0          "
    "      　"
)


def rust_trim_start(text):
    """`text` with Rust's whitespace taken off its start, as `str::trim_start` does."""
    n = 0
    while n < len(text) and text[n] in RUST_WHITESPACE:
        n += 1
    return text[n:]


def is_control(c):
    """Rust's `char::is_control`: the general category Cc."""
    return ord(c) < 0x20 or 0x7F <= ord(c) <= 0x9F


def utf8_len(text):
    """`text`'s length in UTF-8 bytes, as Rust's `str::len` measures it."""
    return len(text.encode("utf-8", "surrogatepass"))


def cut_bytes(text, budget):
    """The longest prefix of `text` of whole characters that fits `budget` UTF-8 bytes."""
    used = 0
    for n, c in enumerate(text):
        used += utf8_len(c)
        if used > budget:
            return text[:n]
    return text


def ascii_lower(text):
    """`text` with ASCII letters lowercased and no other, Rust's `to_ascii_lowercase`."""
    return "".join(chr(ord(c) + 32) if "A" <= c <= "Z" else c for c in text)


def content_type_of(name):
    """The content type of a file named `name`, by its extension in any case."""
    stem, dot, extension = name.rpartition(".")
    extension = ascii_lower(extension) if dot and stem else ""
    return {
        "png": "image/png",
        "jpg": "image/jpeg",
        "jpeg": "image/jpeg",
        "gif": "image/gif",
        "webp": "image/webp",
        "svg": "image/svg+xml",
        "txt": "text/plain",
        "log": "text/plain",
        "md": "text/markdown",
        "json": "application/json",
        "patch": "text/x-diff",
        "diff": "text/x-diff",
        "pdf": "application/pdf",
    }.get(extension, "application/octet-stream")


def escape_link_text(name):
    """`name` as a link's text: every `\\`, `[` and `]` escaped."""
    return "".join("\\" + c if c in "\\[]" else c for c in name)


def embed(text, files):
    """The markdown a comment is sent as: `text` verbatim, then each `(name, content_type,
    url)` of `files` on a paragraph of its own, an image embedded and any other file linked."""
    links = [
        f"{'!' if content_type.startswith('image/') else ''}[{escape_link_text(name)}]({url})"
        for name, content_type, url in files
    ]
    return "\n\n".join(([text] if text else []) + links)


def _link_at(md, start):
    """The link whose text opens at the `[` at `md[start]`: its text, escapes undone, and its
    url; or None."""
    text = []
    depth, escaped, close = 0, False, None
    last = "["
    offset = 0
    for at in range(start + 1, len(md)):
        c = md[at]
        i, offset = offset, offset + utf8_len(c)
        if i > MAX_LINK_TEXT or (c == "\n" and last == "\n"):
            return None
        last = c
        if escaped:
            if c not in string.punctuation:
                text.append("\\")
            text.append(c)
            escaped = False
            continue
        if c == "\\":
            escaped = True
        elif c == "]" and depth == 0:
            close = at
            break
        elif c == "[":
            depth += 1
        elif c == "]":
            depth -= 1
        if c != "\\":
            text.append(c)
    if close is None or not md.startswith("(", close + 1):
        return None
    rest = cut_bytes(md[close + 2 :], MAX_LINK_DESTINATION)
    if rest.startswith("<"):
        inner = rest[1:]
        end = next((n for n, c in enumerate(inner) if c in "><\n"), None)
        if end is None or inner[end] != ">":
            return None
        url, after = inner[:end], inner[end + 1 :]
    else:
        depth, end = 0, len(rest)
        for n, c in enumerate(rest):
            if c == "(":
                if depth == MAX_LINK_PARENS:
                    return None
                depth += 1
            elif c == ")":
                if depth == 0:
                    end = n
                    break
                depth -= 1
            elif c in RUST_WHITESPACE:
                end = n
                break
        url, after = rest[:end], rest[end:]
    after = rust_trim_start(after)
    if not after:
        return None
    if after[0] in "\"'":
        body = after[1:]
        quote = body.find(after[0])
        if quote < 0:
            return None
        after = rust_trim_start(body[quote + 1 :])
    elif after[0] == "(":
        body = after[1:]
        end = next((n for n, c in enumerate(body) if c in "()"), None)
        if end is None or body[end] != ")":
            return None
        after = rust_trim_start(body[end + 1 :])
    elif after[0] != ")":
        return None
    return ("".join(text), url) if after.startswith(")") else None


def linked_files(markdown, is_linear):
    """Every link and image in `markdown` whose url `is_linear` takes, in the order written,
    as `(text, url)`: file.rs's `linked_files`."""
    found = []
    frm = 0
    while True:
        start = markdown.find("[", frm)
        if start < 0:
            return found
        escapes = 0
        while start - escapes > 0 and markdown[start - escapes - 1] == "\\":
            escapes += 1
        if escapes % 2 == 0:
            link = _link_at(markdown, start)
            if link is not None and is_linear(link[1]):
                found.append(link)
        frm = start + 1


def api_root():
    """The API root LINEAR_API_URL is under, its `/graphql` and trailing `/` dropped."""
    url = os.environ.get(ENV_API_URL) or ""
    if url.endswith(GRAPHQL_PATH):
        url = url[: -len(GRAPHQL_PATH)]
    return url.rstrip("/")


def is_linear_file(url, root=None):
    """Whether `url` is a file Linear hosts: under its file host or the API root.
    client.rs's `is_linear_file`."""
    roots = (UPLOADS_BASE, api_root() if root is None else root)
    return any(url.startswith(r) and url[len(r) :].startswith("/") for r in roots if r)


def download(url):
    """The bytes at the Linear file `url`, fetched with the credential; at most
    MAX_FILE_BYTES. A redirect is followed without the credential."""
    stage = "fetch file"
    if not is_linear_file(url):
        raise LinearError(f"linear {stage}: no file Linear hosts at {url}")
    (authorization,) = need_env(ENV_AUTHORIZATION)
    try:
        status, headers, body = _send("GET", url, authorization=authorization, cap=MAX_FILE_BYTES)
    except LinearError as e:
        raise LinearError(f"linear {stage}: {e}") from None
    if status == 429:
        raise LinearError(_rate_limited(stage, headers))
    if not 200 <= status < 300:
        raise LinearError(f"linear {stage}: Linear returned status {status}")
    return body


def put_headers(content_type, signed):
    """The headers an upload's PUT carries: its type and Linear's `Cache-Control`, each
    replaced by a signed header of the same name. client.rs's `put_headers`."""
    headers = [("Content-Type", content_type), ("Cache-Control", "public, max-age=31536000")]
    for name, value in signed:
        headers = [(kept, v) for kept, v in headers if kept.lower() != name.lower()]
        headers.append((name, value))
    return headers


def upload(name, content_type, data):
    """Put `data` up as a file named `name`, as the plugin does: `fileUpload` signs the upload,
    and the bytes are PUT to it with the headers Linear returned and never the credential.
    Returns the asset url the file lives at."""
    stage = "upload file"
    signed = call(
        stage,
        "FileUpload",
        {"contentType": content_type, "filename": name, "size": len(data)},
    )
    target = connection_at(signed, ("fileUpload", "uploadFile"))
    if not isinstance(target, dict):
        raise LinearError(f"linear {stage}: Linear could not prepare the upload")
    upload_url, asset_url = str_at(stage, target, "uploadUrl"), str_at(stage, target, "assetUrl")
    signed_headers = [(h.get("key"), h.get("value")) for h in target.get("headers") or []]
    headers = put_headers(content_type, signed_headers)
    try:
        status, _, _ = _send("PUT", upload_url, data, headers)
    except LinearError as e:
        raise LinearError(f"linear {stage}: {e}") from None
    if not 200 <= status < 300:
        raise LinearError(f"linear {stage}: Linear returned status {status}")
    return asset_url


def write_atomic(path, data):
    """Write `data` to `path` through a temp file beside it renamed onto it, so `path` never
    holds part of a file; the temp file goes with a failure."""
    directory = os.path.dirname(path) or "."
    temp = os.path.join(directory, f".afkd-linear-{os.getpid()}")
    try:
        with open(temp, "wb") as handle:
            handle.write(data)
        os.replace(temp, path)
    except OSError:
        if os.path.exists(temp):
            os.remove(temp)
        raise
