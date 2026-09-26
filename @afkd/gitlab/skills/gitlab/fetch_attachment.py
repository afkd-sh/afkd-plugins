#!/usr/bin/env python3
# fetch_attachment.py — download the active GitLab issue/MR's uploads to scratch (ADR-0017 §7, ADR-0041).
#
# The fetch half of the bundled `gitlab` skill's attachment surface (posting is
# post_attachment.py): a human who pastes a screenshot / log / diagram into an
# issue or a note lands it on GitLab as a markdown `/uploads/<hash>/<file>`
# reference embedded in the body text — the agent can see the *reference* but not
# the *file*. This helper scans the issue/MR **description** and its **notes** for
# those references, downloads each referenced file into
# `$AFKD_SCRATCH_DIR/attachments/<hash>/<basename>`, and prints the written
# path(s), so the agent can then `Read` the file (e.g. view the screenshot). On
# any failure it exits non-zero so the agent surfaces the failure rather than
# retrying blindly.
#
# WHY THE UPLOADS API. A reference in body text is `/uploads/<hash>/<file>`,
# relative to the project's web path, not the `/api/v4` REST base. Rather than
# derive that web path (a `GET /projects/:id` → `web_url` round-trip, with the
# open question of whether the web `/uploads` path wants a session cookie), this
# downloads through GitLab's documented REST endpoint
# `GET /projects/:id/uploads/:secret/:filename` — the reference's `<hash>` is the
# `:secret` and `<file>` is `:filename`. That keeps a SINGLE AUTH SHAPE (the same
# `PRIVATE-TOKEN` header every other helper uses, which is why private-project
# files succeed) and needs no host/scheme reconstruction. Pinned to the
# documented contract, not observed against a live server (the builder container
# cannot reach one) — a wrong endpoint surfaces as a runtime non-2xx, never
# silently.
#
# TRAVERSAL SAFETY. The scan pins `<hash>` to GitLab's actual uploads-hash shape,
# a fixed 32-char lowercase hex token (`[0-9a-f]{32}`), so a crafted reference
# such as `/uploads/../../x/file` simply does not match — `..` is not 32 hex
# chars, so it can never steer a path separator into the `<hash>` directory
# component. The `<basename>` is likewise taken safely (`os.path.basename`, plus a
# guard skipping `""`/`.`/`..`), so BOTH path segments are traversal-safe.
#
# It reads the issue/MR number and credentials FROM THE ENVIRONMENT — the env the
# afkd gitlab trigger already merges into the worker child (crates/gitlab/src/common.rs:
# GITLAB_TOKEN / GITLAB_BASE_URL / GITLAB_PROJECT / GITLAB_MR_NUMBER / GITLAB_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written
# to <scratch>/mr/number or <scratch>/issue/number). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics.

import http.client
import json
import os
import re
import sys
import urllib.parse

# GitLab renders pasted files as `/uploads/<32-hex-secret>/<filename>` markdown
# references. Pinning the secret to `[0-9a-f]{32}` is what makes a crafted
# `/uploads/../../x/file` simply not match; the filename class excludes the
# markdown/HTML delimiters that would end the reference.
UPLOAD_RE = re.compile(r"/uploads/([0-9a-f]{32})/([^\s)\"'<>]+)")


def gitlab_api_base(base_url):
    """Resolve GITLAB_BASE_URL to the REST v4 root every endpoint hangs off, the way
    the Rust client's gitlab_api_base does (crates/gitlab/src/client.rs:336). Empty
    falls back to `https://gitlab.com`; otherwise the trimmed root keeps its scheme;
    a trailing slash is trimmed. Cloud and self-managed are identical (both under
    `/api/v4`)."""
    b = base_url.strip()
    root = "https://gitlab.com" if not b else b.rstrip("/")
    return f"{root}/api/v4"


def encode_project(project):
    """Render GITLAB_PROJECT as its `:id` path segment, the way the Rust client's
    encode_project does (crates/gitlab/src/client.rs:350): an all-ASCII-digit id
    passes through; anything else (a path-with-namespace) is percent-encoded, escaping
    every byte outside the unreserved set (`A–Z a–z 0–9 - _ . ~`), so `group/widgets`
    becomes `group%2Fwidgets`."""
    if project and all(0x30 <= b <= 0x39 for b in project.encode("utf-8")):
        return project
    unreserved = set(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~")
    out = []
    for b in project.encode("utf-8"):
        out.append(chr(b) if b in unreserved else f"%{b:02X}")
    return "".join(out)


def api_path(api_base, suffix):
    """Join the API base's path prefix with an endpoint suffix. The base's path is
    `/api/v4` (→ `/api/v4/projects/…`), so the version prefix rides the resolved
    base — never a literal `/api/v1`."""
    return urllib.parse.urlsplit(api_base).path + suffix


def connection(api_base):
    """Open an HTTP(S) connection to the API host parsed from `api_base`."""
    parts = urllib.parse.urlsplit(api_base)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def resolve_number():
    """The active number and its item kind: GITLAB_MR_NUMBER → `merge_requests`,
    then GITLAB_ISSUE_NUMBER → `issues`, then the <scratch>/mr/number → `merge_requests`
    and <scratch>/issue/number → `issues` file fallbacks, else (None, None). MR wins
    over an issue at every tier, mirroring gitea's PR-over-issue precedence."""
    for var, kind in (("GITLAB_MR_NUMBER", "merge_requests"), ("GITLAB_ISSUE_NUMBER", "issues")):
        number = os.environ.get(var)
        if number:
            return number, kind
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        for rel, kind in (("mr", "merge_requests"), ("issue", "issues")):
            path = os.path.join(scratch, rel, "number")
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as f:
                    return f.read().strip(), kind
    return None, None


def get(api_base, suffix, token):
    """GET `suffix` (joined onto the API base path); return (status, body-bytes).
    The token rides the PRIVATE-TOKEN header, never a query param, so it is not
    surfaced."""
    conn = connection(api_base)
    try:
        conn.request("GET", api_path(api_base, suffix), headers={"PRIVATE-TOKEN": token})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def download(api_base, project, upload_hash, upload_file, token):
    """GET the upload's bytes through the REST uploads endpoint; return (status,
    bytes). The token rides the same PRIVATE-TOKEN header as every other call
    (single auth shape), so private-project files succeed and the token is not
    surfaced."""
    suffix = f"/projects/{encode_project(project)}/uploads/{upload_hash}/{upload_file}"
    return get(api_base, suffix, token)


def references(*bodies):
    """Scan each body for `/uploads/<hash>/<file>` references, returning the unique
    `(hash, file)` pairs in first-seen order. The same reference across several
    notes downloads once; distinct pairs sharing only a basename stay separate
    because the `<hash>` differs."""
    seen = []
    for body in bodies:
        if not body:
            continue
        for upload_hash, upload_file in UPLOAD_RE.findall(body):
            pair = (upload_hash, upload_file)
            if pair not in seen:
                seen.append(pair)
    return seen


def main():
    # --- Credentials + project: required, from the inherited environment. Name what
    #     is missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITLAB_TOKEN")
    base_url = os.environ.get("GITLAB_BASE_URL")
    project = os.environ.get("GITLAB_PROJECT")
    missing = [
        name
        for name, value in (
            ("GITLAB_TOKEN", token),
            ("GITLAB_BASE_URL", base_url),
            ("GITLAB_PROJECT", project),
        )
        if not value
    ]
    if missing:
        print(
            f"fetch_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = gitlab_api_base(base_url)

    # --- The issue/MR number: GITLAB_MR_NUMBER, then GITLAB_ISSUE_NUMBER, then the
    #     scratch file fallbacks. Its origin also selects the path kind. ---
    number, kind = resolve_number()
    if not number:
        print(
            "fetch_attachment.py: no issue/MR number (set GITLAB_MR_NUMBER or "
            "GITLAB_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{mr,issue}/number)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The scratch dir: the download's destination is under it, so it is
    #     required (the trigger always exports it into the run). ---
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if not scratch:
        print(
            "fetch_attachment.py: AFKD_SCRATCH_DIR must be set (the download destination)",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Fetch the issue/MR description, then its notes; a non-2xx on either is a
    #     non-zero exit the agent surfaces. ---
    status, body = get(api_base, f"/projects/{encode_project(project)}/{kind}/{number}", token)
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to read {project}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)
    description = json.loads(body).get("description", "")

    status, body = get(
        api_base, f"/projects/{encode_project(project)}/{kind}/{number}/notes", token
    )
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to read notes for {project}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)
    note_bodies = [note.get("body", "") for note in json.loads(body)]

    # --- Scan the description and every note body for `/uploads/<hash>/<file>`
    #     references. No references anywhere is a success, not an error — analogous
    #     to the "no comments" success path. ---
    refs = references(description, *note_bodies)
    if not refs:
        print(f"no attachments on {project}#{number}")
        sys.exit(0)

    # --- Download each unique reference and write it under
    #     $AFKD_SCRATCH_DIR/attachments/<hash>/<basename>. The `<hash>` is
    #     regex-pinned safe; the `<basename>` is basename-only with an empty/`.`/`..`
    #     guard, so BOTH path segments are traversal-safe. A non-2xx download is a
    #     non-zero exit; the token is never echoed. ---
    for upload_hash, upload_file in refs:
        basename = os.path.basename(upload_file)
        if basename in ("", ".", ".."):
            continue
        status, content = download(api_base, project, upload_hash, upload_file, token)
        if not 200 <= status < 300:
            print(
                f"fetch_attachment.py: failed to download /uploads/{upload_hash}/{upload_file}",
                file=sys.stderr,
            )
            sys.exit(1)
        dest_dir = os.path.join(scratch, "attachments", upload_hash)
        os.makedirs(dest_dir, exist_ok=True)
        dest = os.path.join(dest_dir, basename)
        with open(dest, "wb") as handle:
            handle.write(content)
        print(dest)


if __name__ == "__main__":
    main()
