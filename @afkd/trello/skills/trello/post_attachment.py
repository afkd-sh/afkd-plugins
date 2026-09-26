#!/usr/bin/env python3
# post_attachment.py — attach a local file to a Trello card (ADR-0046 §B).
#
# The post half of the bundled `trello` skill's attachment surface (listing is
# list_attachments.py, fetch is fetch_attachment.py): it uploads one local file as
# an attachment on the card this run works on, so the agent can hand a human the
# artifact itself — an annotated screenshot, a rendered diff, a report — instead of
# describing it. The file path is a REQUIRED positional argument; --name gives the
# attachment an optional display name and --card overrides the target card. On an
# upload failure it exits non-zero so the agent surfaces the failure rather than
# retrying blindly.
#
# AUTH SHAPE (finding 1 of ADR-0046, validated live): the upload POSTs a
# `multipart/form-data` body with the creds in the QUERY STRING (unlike the
# download in fetch_attachment.py, which uses the OAuth header). The file part is
# named `file` (Trello's field); an optional `name` text part rides alongside.
# It posts NO `[afkd-note]` marker — and neither does any other afkd surface any
# more (post_comment.py stopped too): a comment is recognized as afkd's own by the
# posting member id, not by its text. An attachment is an artifact, not
# conversation, so it never enters the comment-delta bookkeeping either way.
#
# It reads the card id and board credentials FROM THE ENVIRONMENT — the env the
# afkd trello trigger already merges into the worker child
# (crates/trello/src/trigger.rs: TRELLO_API_KEY / TRELLO_TOKEN / TRELLO_CARD_ID).
# The card is --card if given, else $TRELLO_CARD_ID; there is no scratch fallback.
#
# Like send_file.py it uses only the Python standard library (no curl): the stdlib
# does not build multipart bodies, so the body is assembled by hand (uuid4 boundary,
# binary-safe file part), and a ~10 MB file guard fails early before any upload.
#
# TRELLO_API_BASE overrides the API host. It defaults to the production host, so
# field behavior is unchanged; the override exists ONLY so tests can point the
# upload at a local stub listener, mirroring the seam in list_comments.py.

import argparse
import http.client
import os
import sys
import urllib.parse
import uuid

# Trello caps a single attachment upload at ~10 MB; guard locally so an oversized
# file fails early with a clear message instead of a doomed upload. The server
# stays the ultimate authority — a server-side rejection still surfaces as a loud
# fail.
MAX_FILE_BYTES = 10 * 1024 * 1024


def connection(base):
    """Open an HTTP(S) connection to the API host parsed from `base`."""
    parts = urllib.parse.urlsplit(base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def multipart_body(fields, filename, content):
    """Build a `multipart/form-data` body by hand; return (content_type, bytes).

    The stdlib does not assemble multipart bodies, so we do: a random boundary
    (`uuid4().hex`, so it cannot collide with the file content), one text part per
    item in `fields` (the optional display name), and one file part named `file`
    (Trello's field) carrying `content` VERBATIM as application/octet-stream. Text
    parts are UTF-8 encoded; the file bytes are placed unchanged (binary-safe).
    """
    boundary = uuid.uuid4().hex
    marker = f"--{boundary}".encode()
    parts = []
    for name, value in fields:
        parts.append(marker)
        parts.append(f'Content-Disposition: form-data; name="{name}"'.encode())
        parts.append(b"")
        parts.append(value.encode("utf-8"))
    parts.append(marker)
    parts.append(
        f'Content-Disposition: form-data; name="file"; filename="{filename}"'.encode()
    )
    parts.append(b"Content-Type: application/octet-stream")
    parts.append(b"")
    parts.append(content)
    parts.append(f"--{boundary}--".encode())
    parts.append(b"")
    body = b"\r\n".join(parts)
    return f"multipart/form-data; boundary={boundary}", body


def post_attachment(base, card, name, filename, content, query):
    """POST one attachment to the card; return (status, body-bytes).

    Creds ride in the query string (finding 1); the optional display `name`, the
    file name, and the file bytes travel in the multipart body. The token is never
    echoed.
    """
    fields = []
    if name:
        fields.append(("name", name))
    content_type, body = multipart_body(fields, filename, content)
    conn = connection(base)
    try:
        conn.request(
            "POST",
            f"/1/cards/{card}/attachments?{query}",
            body,
            {"Content-Type": content_type},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # nargs="?" so argparse does not emit its own error on a missing path; the
    # hand-rolled guard below preserves a clear wording and a dedicated exit code.
    parser.add_argument("path", nargs="?")
    parser.add_argument("--name", help="optional display name for the attachment")
    parser.add_argument("--card", help="card id (defaults to $TRELLO_CARD_ID)")
    args = parser.parse_args()

    # --- The file path: the single positional argument. ---
    if not args.path:
        print(
            "post_attachment.py: a file path is required (pass it as the only positional argument)",
            file=sys.stderr,
        )
        sys.exit(2)

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
            f"post_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The card id: --card, else TRELLO_CARD_ID. No scratch fallback. ---
    card = args.card or os.environ.get("TRELLO_CARD_ID")
    if not card:
        print(
            "post_attachment.py: no card id (set TRELLO_CARD_ID or pass --card)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Pre-flight file validation, before opening/reading, so a bad path never
    #     reaches the network. Size is read from the stat, so an oversized file is
    #     rejected without reading it into memory and without any upload. ---
    path = args.path
    if not os.path.exists(path):
        print(f"post_attachment.py: no such file: {path}", file=sys.stderr)
        sys.exit(1)
    if not os.path.isfile(path):
        print(f"post_attachment.py: not a regular file: {path}", file=sys.stderr)
        sys.exit(1)
    if os.path.getsize(path) > MAX_FILE_BYTES:
        print(
            f"post_attachment.py: file is larger than the 10 MB Trello attachment cap: {path}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The API host: production by default, overridable only for tests. ---
    base = os.environ.get("TRELLO_API_BASE", "https://api.trello.com")

    # --- Read the file bytes (binary, verbatim) and upload. Trello infers the
    #     mimeType server-side; the card sees the basename as the file name, and
    #     --name (if given) as the display name. The token is never echoed. ---
    filename = os.path.basename(path)
    with open(path, "rb") as handle:
        content = handle.read()
    query = urllib.parse.urlencode({"key": key, "token": token})
    status, _ = post_attachment(base, card, args.name, filename, content, query)
    if not 200 <= status < 300:
        print(
            f"post_attachment.py: failed to post {filename} to card {card}",
            file=sys.stderr,
        )
        sys.exit(1)

    print(f"post_attachment.py: posted {filename} to card {card}")


if __name__ == "__main__":
    main()
