#!/usr/bin/env python3
# fetch_attachment.py — download one Trello card attachment (ADR-0046 §B).
#
# The fetch half of the bundled `trello` skill's attachment surface (listing is
# list_attachments.py): it downloads a single attachment and prints the written
# path, so the agent can then `Read` the file (e.g. see a screenshot a human pasted
# onto the card, or an earlier attempt's plan). It names the attachment by its id
# (the positional argument) or by --name, the newest uploaded file of exactly that
# name — the same pick the plugin's `fetch_attachment` action makes. It writes into
# `$AFKD_SCRATCH_DIR/attachments/` under the attachment's own name, or to --to,
# through a temp file renamed into place so the path never holds part of a file.
# With --name and no attachment of that name it says so and exits 0, writing
# nothing. The card defaults from the environment. On any other failure it exits
# non-zero so the agent surfaces the failure rather than retrying blindly.
#
# TWO REQUESTS, TWO AUTH SHAPES (finding 2 of ADR-0046, validated live):
#   1. Metadata GET with QUERY creds — an ordinary read, yielding the attachment's
#      `name`, `url`, and `bytes`: `/1/cards/{card}/attachments/{att-id}` for an
#      id, or the card's whole listing `/1/cards/{card}/attachments` for --name.
#   2. Download GET of that `url` VERBATIM, authenticated with the OAuth
#      `Authorization` header (`OAuth oauth_consumer_key=…, oauth_token=…`) and NO
#      query creds. Trello's attachment-download host rejects query-string creds;
#      only the OAuth header works. The url is used exactly as returned (its host
#      differs from the API host), so this helper never reconstructs it.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir, which --to makes needless).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# TRELLO_API_BASE overrides the metadata API host. It defaults to the production
# host, so field behavior is unchanged; the override exists ONLY so tests can point
# the metadata fetch at a local stub listener, mirroring the seam in
# list_comments.py. The download host always comes from the returned `url`.

import argparse
import http.client
import json
import os
import sys
import urllib.parse


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def get_json(base, path, query):
    """GET `path` with `query`; return (status, body-bytes)."""
    conn = connection(base)
    try:
        conn.request("GET", f"{path}?{query}")
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def newest_named(attachments, name):
    """The newest uploaded file named exactly `name`, or None.

    Newest is the greatest (date, id): Trello's dates are ISO-8601 in UTC with
    milliseconds, so they sort as text to the millisecond. Ids do not order two
    uploads in one second, so the id only settles a tie in the same millisecond.
    A link is no file, so a link of that name is passed over.
    """
    files = [
        a
        for a in attachments
        if a.get("isUpload") and a.get("name") == name and a.get("url")
    ]
    if not files:
        return None
    return max(files, key=lambda a: (a.get("date") or "", a.get("id") or ""))


def write_atomically(dest, content):
    """Write `content` to `dest` through a sibling temp file renamed onto it."""
    directory, base = os.path.split(dest)
    temp = os.path.join(directory, f".{base}.fetch_attachment-{os.getpid()}")
    try:
        with open(temp, "wb") as handle:
            handle.write(content)
        os.replace(temp, dest)
    except OSError:
        if os.path.exists(temp):
            os.remove(temp)
        raise


def download(url, key, token):
    """GET the attachment `url` VERBATIM with the OAuth header; return (status, bytes).

    Trello's download host authenticates the file transfer with the OAuth
    `Authorization` header, NOT query-string creds (finding 2). The url is parsed
    only to split host from path+query; the path+query is sent unchanged, so no
    cred ever rides the download's query string. The token is never echoed.
    """
    parts = urllib.parse.urlsplit(url)
    target = parts.path
    if parts.query:
        target += f"?{parts.query}"
    header = {
        "Authorization": f'OAuth oauth_consumer_key="{key}", oauth_token="{token}"'
    }
    if parts.scheme == "https":
        conn = http.client.HTTPSConnection(parts.hostname, parts.port)
    else:
        conn = http.client.HTTPConnection(parts.hostname, parts.port)
    try:
        conn.request("GET", target, headers=header)
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # An id or a name, exactly one: argparse emits its own error (exit 2) for both
    # or neither, before any network.
    parser.add_argument(
        "att_id", nargs="?", help="the attachment id (from list_attachments.py)"
    )
    parser.add_argument(
        "--name", help="fetch the newest uploaded file named exactly this instead"
    )
    parser.add_argument(
        "--to", help="write here instead of $AFKD_SCRATCH_DIR/attachments/<name>"
    )
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()
    if (args.att_id is None) == (args.name is None):
        parser.error("name the attachment by its id or by --name, not both")

    # --- Credentials: required, from the inherited environment. Name what is
    #     missing so the config author knows which var to wire (ADR-0023). ---
    key = os.environ.get("TRELLO_API_KEY")
    token = os.environ.get("TRELLO_TOKEN")
    missing = []
    if not key:
        missing.append("TRELLO_API_KEY")
    if not token:
        missing.append("TRELLO_TOKEN")
    if missing:
        print(
            f"fetch_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "fetch_attachment.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The scratch dir: without --to the download's destination is under it, so
    #     it is required then (the trigger always exports it into the run). ---
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if not args.to and not scratch:
        print(
            "fetch_attachment.py: AFKD_SCRATCH_DIR must be set (the download destination), "
            "or pass --to",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Request 1: the metadata, with QUERY creds (an ordinary read). It yields
    #     the download url and the file name we write under. ---
    if args.name is not None:
        query = urllib.parse.urlencode(
            {
                "key": key,
                "token": token,
                "fields": "id,name,date,url,isUpload,bytes",
            }
        )
        status, body = get_json(base, f"/1/cards/{card}/attachments", query)
        if not 200 <= status < 300:
            print(
                f"fetch_attachment.py: failed to list the attachments on card {card}",
                file=sys.stderr,
            )
            sys.exit(1)
        meta = newest_named(json.loads(body), args.name)
        if meta is None:
            print(f"fetch_attachment.py: no attachment named {args.name} on card {card}")
            return
        what = f"{args.name!r}"
    else:
        query = urllib.parse.urlencode({"key": key, "token": token})
        status, body = get_json(
            base, f"/1/cards/{card}/attachments/{args.att_id}", query
        )
        if not 200 <= status < 300:
            print(
                f"fetch_attachment.py: failed to read attachment {args.att_id} on card {card}",
                file=sys.stderr,
            )
            sys.exit(1)
        meta = json.loads(body)
        what = args.att_id
    url = meta.get("url")
    name = meta.get("name")
    if not url or not name:
        print(
            f"fetch_attachment.py: attachment {what} has no downloadable url",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Request 2: the bytes, from the url VERBATIM with the OAuth header and NO
    #     query creds (finding 2). A non-2xx is a non-zero exit the agent surfaces. ---
    status, content = download(url, key, token)
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to download attachment {what}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Write to --to, or into $AFKD_SCRATCH_DIR/attachments/ under the
    #     attachment's own name (basename only, so a crafted name can't escape the
    #     directory). ---
    if args.to:
        dest = args.to
    else:
        dest_dir = os.path.join(scratch, "attachments")
        os.makedirs(dest_dir, exist_ok=True)
        dest = os.path.join(dest_dir, os.path.basename(name))
    try:
        write_atomically(dest, content)
    except OSError as e:
        print(f"fetch_attachment.py: cannot write {dest}: {e}", file=sys.stderr)
        sys.exit(1)

    print(dest)


if __name__ == "__main__":
    main()
