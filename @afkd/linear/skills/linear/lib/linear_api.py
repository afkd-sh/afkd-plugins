# linear_api.py — what every script of the bundled `linear` skill shares.
#
# Each script parses its arguments and prints; this module holds the rest, once: the
# environment a run carries, the GraphQL requests, paging, resolving a state or a label by id
# or by name, placing an issue in a state's manual order, and the file half — reading the
# files an issue links, naming them as the plugin names them, and putting one up.
#
# THE ENVIRONMENT is the one the plugin puts on every run (src/common.rs): LINEAR_API_URL, the
# GraphQL endpoint; LINEAR_AUTHORIZATION, the exact `Authorization` value (an API key as it
# is, or `Bearer <token>` for an app); LINEAR_TEAM_ID; LINEAR_ISSUE_ID, the active issue's id;
# and LINEAR_ISSUE, its identifier (`ENG-123`). LINEAR_API_URL is also the seam the tests
# point at a fake Linear. A script that needs one that is unset exits 1 naming it, before any
# request.
#
# EVERY REQUEST is one named operation out of operations.graphql beside this file, sent by
# its name, as the plugin sends its own: no script builds a GraphQL string. Linear answers a
# request it refuses with an `errors` body, on a 200 as well as on a 4xx, so every reply is
# read for `errors` before its `data`. A rate limit is said with the wait Linear asked for and
# never retried: the agent surfaces it.
#
# THE FILE HALF ports src/file.rs, function for function, and src/file.rs's parity test holds
# the two to one fixture: what `list_files.py` lists and `fetch_file.py` writes is what the
# plugin's `fetch_files` writes, and what `post_file.py` posts is what its `files=` posts. Rust
# counts a string in UTF-8 bytes and Python in characters, so each bound is measured in bytes
# here, and the two character classes Python spells otherwise are spelled out as Rust defines
# them.
#
# Python 3.9 or newer, and the standard library only. Not executable: the scripts import it.

import argparse
import http.client
import json
import os
import string
import sys
import urllib.error
import urllib.request

# --- the environment -------------------------------------------------------------------

ENV_API_URL = "LINEAR_API_URL"
ENV_AUTHORIZATION = "LINEAR_AUTHORIZATION"
ENV_TEAM_ID = "LINEAR_TEAM_ID"
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


def positive_int(text):
    """An argparse type: a whole number of at least 1."""
    try:
        value = int(text)
    except ValueError:
        value = 0
    if value < 1:
        raise argparse.ArgumentTypeError(f"{text!r} is not a whole number of at least 1")
    return value


def read_text_file(path):
    """The UTF-8 text of the file at `path`; exits 1 naming it when it cannot be read."""
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read()
    except (OSError, UnicodeDecodeError) as e:
        die(f"cannot read {path}: {e}")


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


def add_team_arg(parser):
    """Give `parser` the `--team` override of the run's team."""
    parser.add_argument(
        "--team",
        metavar="ID",
        help="the team states and labels are read from (defaults to $LINEAR_TEAM_ID)",
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


def moving_refs(args):
    """Every name the issue the script acts on goes by that it knows: the reference it was
    given, and both the id and the identifier when that is the run's own issue."""
    ref = issue_ref(args)
    own = (os.environ.get(ENV_ISSUE_ID), os.environ.get(ENV_ISSUE))
    return {ref} | ({r for r in own if r} if ref in own else set())


def team_ref(args):
    """The team states and labels are read from: `--team`, else `$LINEAR_TEAM_ID`."""
    team = args.team or os.environ.get(ENV_TEAM_ID)
    if not team:
        die(f"no team (set {ENV_TEAM_ID} or pass --team)")
    return team


def refuse_placeholder(text, placeholder):
    """Exit 2 when `text` is SKILL.md's placeholder, before any request."""
    if text == placeholder:
        die("refusing to send the placeholder text; substitute your own", 2)


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
        raise LinearError(f"no response from Linear ({reason})") from None


def _rate_limited(stage, headers):
    """The sentence for a 429: the wait Linear asked for, when it named one."""
    wait = (headers.get("Retry-After") or "").strip() if headers else ""
    if wait.isdigit():
        return f"{stage}: Linear rate limited it; retry after {wait}s"
    return f"{stage}: Linear rate limited it"


def call(stage, name, variables):
    """Send the operation `name` with `variables` and return its `data`.

    Raises LinearError with the sentence `<stage>: <why>` for a reply that carries no data:
    Linear's own error, a rate limit, a status with no error, or an undecodable body.
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
        raise LinearError(f"{stage}: {e}") from None
    ok = 200 <= status < 300
    if status == 429:
        raise LinearError(_rate_limited(stage, headers))
    try:
        reply = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as e:
        if ok:
            raise LinearError(f"{stage}: undecodable response ({e})") from None
        reply = None
    errors = reply.get("errors") if isinstance(reply, dict) else None
    if isinstance(errors, list) and errors:
        first = errors[0] if isinstance(errors[0], dict) else {}
        message = first.get("message") or "an error with no message"
        if message.startswith("Entity not found"):
            raise LinearError(f"{stage}: {message}")
        raise LinearError(f"{stage}: Linear refused it ({status}): {message}")
    if not ok:
        raise LinearError(f"{stage}: Linear returned status {status}")
    data = reply.get("data") if isinstance(reply, dict) else None
    if not isinstance(data, dict):
        raise LinearError(f"{stage}: undecodable response (no data)")
    return data


def dig(stage, data, path):
    """The value at `path` under `data`, or a LinearError naming the part that is missing."""
    value = data
    for key in path:
        if not isinstance(value, dict) or value.get(key) is None:
            raise LinearError(f"{stage}: undecodable response (no {'.'.join(path)})")
        value = value[key]
    return value


def pages(stage, name, variables, path):
    """Every node of the connection at `path`, page after page, from `after`."""
    variables = dict(variables)
    while True:
        connection = dig(stage, call(stage, name, variables), path)
        yield from connection.get("nodes") or []
        info = connection.get("pageInfo") or {}
        if not info.get("hasNextPage") or not info.get("endCursor"):
            return
        variables["after"] = info["endCursor"]


# --- states, labels and the issues in a state --------------------------------------------


def states(team):
    """The team's workflow states, in the order its workflow shows them."""
    data = call("read states", "TeamStates", {"teamId": team})
    nodes = dig("read states", data, ("team", "states")).get("nodes") or []
    return sorted(nodes, key=lambda s: s.get("position") or 0)


def labels(team):
    """Every label an issue of the team can carry: the team's and the workspace's."""
    label_filter = {"or": [{"team": {"id": {"eq": team}}}, {"team": {"null": True}}]}
    return list(pages("read labels", "TeamLabels", {"filter": label_filter}, ("issueLabels",)))


def resolve(what, items, ref, team):
    """The one of `items` (states or labels) that `ref` names: the one whose id it is, else
    the one whose name it is exactly. No match, or a name two of them carry, exits 1."""
    for item in items:
        if item.get("id") == ref:
            return item
    named = [item for item in items if item.get("name") == ref]
    if len(named) == 1:
        return named[0]
    if not named:
        known = ", ".join(repr(item.get("name")) for item in items) or "none"
        die(f"no {what} {ref!r} on team {team} (it has {known})")
    ids = ", ".join(item.get("id") for item in named)
    die(f"{len(named)} {what}s are named {ref!r} ({ids}); pass the id")


def state_issues(team, state_id, limit=None):
    """The open issues in a state, in its manual order, at most `limit` of them."""
    variables = {
        "filter": {"team": {"id": {"eq": team}}, "state": {"id": {"eq": state_id}}},
        "sort": [{"manual": {"order": "Ascending"}}],
        "first": 50 if limit is None else min(50, limit),
    }
    found = []
    for node in pages("read issues", "StateIssues", variables, ("issues",)):
        found.append(node)
        if limit is not None and len(found) >= limit:
            break
    return found


def _names(issue, ref):
    """Whether `ref` names `issue`, by id or by identifier."""
    return ref in (issue.get("id"), issue.get("identifier"))


def sort_order_for(team, state, pos, above, below, moving=()):
    """The `sortOrder` that puts an issue where it was asked for in `state`.

    `pos` is an end: the bottom is the last issue's + 1000, or 1000 in an empty state, as
    the plugin puts back an issue it gave up, and the top the first's - 1000, or -1000.
    `above` or `below` names an issue in the state, and the answer is the mean of its
    `sortOrder` and its neighbour's, or the end when it is at that end. The issue moving,
    named by any of `moving` (its id and identifier, none for a new issue), is left out of
    the neighbours. An anchor that is the issue moving exits 2; one not in the state exits 1.
    """
    anchor = above if above is not None else below
    issues = state_issues(team, state["id"])

    def is_moving(issue):
        return any(_names(issue, ref) for ref in moving)

    if anchor in moving or any(_names(i, anchor) and is_moving(i) for i in issues):
        die(f"cannot place {anchor} relative to itself", 2)
    issues = [i for i in issues if not is_moving(i)]
    orders = [float(i.get("sortOrder") or 0) for i in issues]
    if anchor is None:
        if pos == "top":
            return orders[0] - 1000 if orders else -1000.0
        return orders[-1] + 1000 if orders else 1000.0
    at = next((n for n, issue in enumerate(issues) if _names(issue, anchor)), None)
    if at is None:
        die(f"no issue {anchor} in state {state.get('name')!r}")
    if above is not None and at == 0:
        return orders[0] - 1000
    if below is not None and at == len(orders) - 1:
        return orders[-1] + 1000
    lo, hi = (orders[at - 1], orders[at]) if above is not None else (orders[at], orders[at + 1])
    order = (lo + hi) / 2
    if not lo < order < hi:
        die(f"no place left between {anchor} and its neighbour; move one of them first")
    return order


def add_placement_args(parser, default):
    """Give `parser` the three exclusive ways to place an issue: `--pos`, `--above`, `--below`."""
    place = parser.add_mutually_exclusive_group()
    place.add_argument("--pos", choices=["top", "bottom"], default=default)
    place.add_argument("--above", metavar="ISSUE", help="put it directly above ISSUE")
    place.add_argument("--below", metavar="ISSUE", help="put it directly below ISSUE")


def placed(args):
    """How the arguments place the issue, as the output says it."""
    if args.above is not None:
        return f"above {args.above}"
    if args.below is not None:
        return f"below {args.below}"
    return f"at the {args.pos}"


# --- the thread ------------------------------------------------------------------------

# The control markers the plugin writes on an issue (src/claim.rs, src/issue.rs). A comment
# opening with one is afkd's bookkeeping, which `fetch_files` reads no file out of.
CONTROL_MARKERS = ("[afkd-claim]", "[afkd-attempt]", "[afkd-ran]", "[afkd-park]")


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
    return rust_trim_start(unescape(text)).startswith(CONTROL_MARKERS)


def thread(issue):
    """The issue's comments, oldest first, as `(createdAt, id)` orders them.

    Linear writes every time in UTC to the millisecond, so the text sorts as the time does.
    """
    nodes = pages("read comments", "IssueComments", {"id": issue}, ("issue", "comments"))
    return sorted(nodes, key=lambda c: (c.get("createdAt") or "", c.get("id") or ""))


def author(comment):
    """Who wrote a comment: a user's display name, a bot's name, or an external user's."""
    for key, field in (("user", "displayName"), ("botActor", "name"), ("externalUser", "name")):
        who = (comment.get(key) or {}).get(field)
        if who:
            return who
    return "unknown"


def issue_label_ids(issue):
    """The ids of the labels on the issue."""
    data = call("read labels", "IssueLabelIds", {"id": issue})
    nodes = dig("read labels", data, ("issue", "labels")).get("nodes") or []
    return {node.get("id") for node in nodes}


def description(issue):
    """The issue's description, "" for none."""
    data = call("read description", "IssueDescription", {"id": issue})
    return (data.get("issue") or {}).get("description") or ""


# --- files: ports of src/file.rs ---------------------------------------------------------

# The most one file may hold, either way: file.rs's MAX_FILE_BYTES.
MAX_FILE_BYTES = 10 * 1024 * 1024

# The bounds file.rs's `linked_files` reads a link within, each in UTF-8 bytes.
MAX_LINK_TEXT = 1024
MAX_LINK_DESTINATION = 2048
MAX_LINK_PARENS = 32

# A written name's bound, and the longest extension a cut keeps, in UTF-8 bytes.
NAME_MAX = 255
MAX_EXTENSION = 16

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


def rust_trim(text):
    """`text` with Rust's whitespace taken off both ends, as `str::trim` does."""
    text = rust_trim_start(text)
    end = len(text)
    while end > 0 and text[end - 1] in RUST_WHITESPACE:
        end -= 1
    return text[:end]


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


def _cut_to(name, budget):
    """`name` cut to at most `budget` bytes of whole characters, its extension of up to
    MAX_EXTENSION bytes kept after the cut stem."""
    if utf8_len(name) <= budget:
        return name
    dot = name.rfind(".")
    extension = name[dot:] if dot >= 0 and utf8_len(name[dot:]) <= MAX_EXTENSION else ""
    return cut_bytes(name, budget - utf8_len(extension)) + extension


def file_name_for(n, width, name, url):
    """The name the `n`th of the files fetched is written under, `n` padded to `width`
    digits: file.rs's `file_name_for`."""
    name = rust_trim(name)
    if not name:
        cut = min((at for at in (url.find("?"), url.find("#")) if at >= 0), default=len(url))
        name = url[:cut].rsplit("/", 1)[-1]
    safe = "".join("_" if c in "/\\" or is_control(c) else c for c in name).lstrip(".")
    prefix = f"{n:0{width}d}-"
    return prefix + _cut_to(safe or "file", NAME_MAX - len(prefix))


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


def issue_files(issue):
    """Every file Linear hosts that the issue links, as `(name, url)`: in its description,
    then in its thread oldest first, control markers left out, each url once. The files
    `fetch_files` writes, in its order."""
    bodies = [c.get("body") or "" for c in thread(issue)]
    texts = [description(issue)] + [b for b in bodies if not is_control_marker(b)]
    files = []
    for text in texts:
        for name, url in linked_files(text, is_linear_file):
            if all(seen != url for _, seen in files):
                files.append((name, url))
    return files


def download(url):
    """The bytes at the Linear file `url`, fetched with the credential; at most
    MAX_FILE_BYTES. A redirect is followed without the credential."""
    if not is_linear_file(url):
        raise LinearError(f"fetch file: no file Linear hosts at {url}")
    (authorization,) = need_env(ENV_AUTHORIZATION)
    try:
        status, headers, body = _send("GET", url, authorization=authorization, cap=MAX_FILE_BYTES)
    except LinearError as e:
        raise LinearError(f"fetch file: {e}") from None
    if status == 429:
        raise LinearError(_rate_limited("fetch file", headers))
    if not 200 <= status < 300:
        raise LinearError(f"fetch file: Linear returned status {status}")
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
    target = dig(stage, signed, ("fileUpload", "uploadFile"))
    upload_url, asset_url = target.get("uploadUrl"), target.get("assetUrl")
    if not upload_url or not asset_url:
        raise LinearError(f"{stage}: undecodable response (no upload url)")
    signed_headers = [(h.get("key"), h.get("value")) for h in target.get("headers") or []]
    headers = put_headers(content_type, signed_headers)
    try:
        status, _, _ = _send("PUT", upload_url, data, headers)
    except LinearError as e:
        raise LinearError(f"{stage}: {e}") from None
    if not 200 <= status < 300:
        raise LinearError(f"{stage}: the storage returned status {status}")
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
