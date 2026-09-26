#!/usr/bin/env python3
# fetch_attachment.py — download the active GitHub issue/PR's embedded images to scratch (ADR-0017 §7, ADR-0041).
#
# The inbound half of the bundled `github` skill's attachment surface — and the
# ONLY half: GitHub exposes no public REST API to upload/post an attachment onto
# an issue or comment, so there is no `post_attachment.py` sibling (the agent
# hands a picture back by hosting it externally and linking it in a comment). A
# human who pastes a screenshot / stack-trace image / diagram into an issue or a
# comment lands it on GitHub as an embedded image URL in the body markdown — the
# agent can see the *reference* but not the *picture*. This helper scans the
# issue/PR **body** and its **comments** for those URLs, downloads each into
# `$AFKD_SCRATCH_DIR/attachments/<token>/<basename>`, and prints the written
# path(s), so the agent can then `Read` the file (the harness renders PNG/JPG
# visually). On any failure it exits non-zero so the agent surfaces the failure
# rather than retrying blindly.
#
# RECOGNITION — PATH SHAPE, NOT HOST. GitHub embeds pasted images as one of two
# absolute URL shapes: `…/user-attachments/assets/<uuid>` (a bare UUID, no
# filename) and `user-images.githubusercontent.com/…/<file.ext>`. Unlike the
# API reads, these live on hosts DISTINCT from the resolved API base, so the
# download cannot hang off that base. The scan matches an absolute `http(s)://`
# URL that CONTAINS either marker — the `/user-attachments/assets/<uuid>` path
# (uuid pinned to the canonical shape) or the literal `user-images.
# githubusercontent.com/` substring — and the download then connects to the
# MATCHED URL's OWN scheme/host/port (never a hard-pinned github.com). Matching on
# the path shape rather than the host is what keeps the real-GitHub URLs working
# while letting a test point every hop at a loopback host that carries the same
# marker in its path.
#
# AUTH — TOKEN ON THE FIRST HOP ONLY. The token rides the `Authorization: Bearer`
# header on the first (GitHub-owned) download hop — harmless for a public asset,
# required for a private one — never a query param, and is never echoed. GitHub
# answers a `user-attachments/assets/<uuid>` URL with a 302 to signed blob storage
# that must NOT receive the token, so the helper follows redirects (a small bound)
# and DROPS the `Authorization` header on every redirected hop (the User-Agent
# rides every hop — GitHub REST 403s a UA-less request).
#
# TRAVERSAL SAFETY + RENDERABLE NAMES. Each download is written under
# `attachments/<token>/<basename>` where `<token>` is a fixed-length hex digest of
# the full URL (`hashlib.sha256(url)[:16]`): it has no path separator, so a
# crafted URL can never steer a separator into that directory component or escape
# `attachments/`, and it is unique per distinct URL (distinct URLs never clobber,
# even sharing a basename) yet identical for an identical URL (idempotent dedup).
# The `<basename>` is basename-only (`os.path.basename`, plus an empty/`.`/`..`
# guard), so BOTH path segments are traversal-safe. The `user-images…/<file.ext>`
# shape already yields a renderable name; the bare-`<uuid>` shape has no
# extension, so one is derived from the download's response headers
# (Content-Disposition filename → a Content-Type→extension map → a `.bin`
# default) so a real image still renders via `Read`.
#
# It reads the issue/PR number and credentials FROM THE ENVIRONMENT — the env the
# afkd github trigger already merges into the worker child (crates/github/src/common.rs:
# GITHUB_TOKEN / GITHUB_HOST / GITHUB_REPO / GITHUB_PR_NUMBER / GITHUB_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir; the number is also written
# to <scratch>/pr/number or <scratch>/issue/number). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics. Like the gitlab helper, the URL shapes and
# the auth/redirect behaviour are pinned to GitHub's documented contract, not
# observed against a live server (the builder container cannot reach github.com) —
# a wrong endpoint surfaces as a runtime non-2xx, never silently.

import hashlib
import http.client
import json
import os
import re
import sys
import urllib.parse

# A non-empty User-Agent is mandatory on GitHub REST — http.client sends none by
# default and real GitHub 403s a UA-less request (crates/github/src/client.rs).
USER_AGENT = "afkd-github-skill"

# GitHub's two embedded-image URL shapes, matched on the path shape (see the
# header): a lazy prefix consumes the scheme/host, then EITHER marker must appear
# — the `/user-attachments/assets/<uuid>` path (uuid pinned to the canonical
# 8-4-4-4-12 hex shape, so a crafted `…/assets/../../x` simply does not match) or
# the literal `user-images.githubusercontent.com/` host substring — then the rest
# of the URL runs to the markdown/HTML delimiter (`[^\s)"'<>]`).
_UUID = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}"
ATTACHMENT_RE = re.compile(
    r"https?://[^\s)\"'<>]*?"
    r"(?:/user-attachments/assets/" + _UUID + r"|user-images\.githubusercontent\.com/)"
    r"[^\s)\"'<>]*"
)

# The status codes that carry a `Location` we follow (dropping auth on the hop).
_REDIRECT_STATUSES = (301, 302, 303, 307, 308)

# Content-Type → file extension for an extensionless download (the bare-UUID
# assets shape), so a real image lands with a name the Read tool renders.
_CONTENT_TYPE_EXT = {
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "image/gif": ".gif",
    "image/webp": ".webp",
    "image/svg+xml": ".svg",
}


def github_api_base(host):
    """Resolve GITHUB_HOST to the REST API root every endpoint hangs off, the way
    the Rust client's github_api_base does (crates/github/src/client.rs:308). Cloud
    — empty or (scheme-insensitively) `github.com` — is `https://api.github.com`
    (paths hang directly off it: `/repos/…`); any other host is GitHub Enterprise
    Server, whose API lives under `/api/v3` of the host (the scheme is preserved
    when present, else `https://` is prepended, and a trailing slash is trimmed)."""
    host = host.strip()
    bare = host
    for scheme in ("https://", "http://"):
        if bare.startswith(scheme):
            bare = bare[len(scheme) :]
            break
    bare = bare.rstrip("/")
    if not host or bare == "github.com":
        return "https://api.github.com"
    if host.startswith(("http://", "https://")):
        with_scheme = host.rstrip("/")
    else:
        with_scheme = f"https://{bare}"
    return f"{with_scheme}/api/v3"


def api_path(api_base, suffix):
    """Join the API base's path prefix with an endpoint suffix. Cloud's base has an
    empty path (→ `/repos/…`); a GHES/loopback base's path is `/api/v3`
    (→ `/api/v3/repos/…`), so the version prefix rides the resolved base — never a
    literal `/api/v1`."""
    return urllib.parse.urlsplit(api_base).path + suffix


def connection(url):
    """Open an HTTP(S) connection to the host parsed from `url` — the resolved API
    base for a read, or a matched attachment URL for a download."""
    parts = urllib.parse.urlsplit(url)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(parts.hostname, parts.port)
    return http.client.HTTPConnection(parts.hostname, parts.port)


def issue_number():
    """The issue/PR number: GITHUB_PR_NUMBER, then GITHUB_ISSUE_NUMBER, then the
    <scratch>/pr/number → <scratch>/issue/number file fallbacks, else None."""
    number = os.environ.get("GITHUB_PR_NUMBER") or os.environ.get("GITHUB_ISSUE_NUMBER")
    if number:
        return number
    scratch = os.environ.get("AFKD_SCRATCH_DIR")
    if scratch:
        for rel in ("pr", "issue"):
            path = os.path.join(scratch, rel, "number")
            if os.path.isfile(path):
                with open(path, encoding="utf-8") as f:
                    return f.read().strip()
    return None


def get(api_base, suffix, token):
    """GET `suffix` (joined onto the API base path); return (status, body-bytes).
    The token rides the Authorization header, never a query param, so it is not
    surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "GET",
            api_path(api_base, suffix),
            headers={
                "Authorization": f"Bearer {token}",
                "User-Agent": USER_AGENT,
            },
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def references(*bodies):
    """Scan each body for the two GitHub embedded-image URL shapes, returning the
    unique full URLs in first-seen order. The same URL referenced twice (across
    the body and its comments) downloads once; distinct URLs stay separate even
    when they share a basename, because the `<token>` is hashed from the full
    URL."""
    seen = []
    for body in bodies:
        if not body:
            continue
        for url in ATTACHMENT_RE.findall(body):
            if url not in seen:
                seen.append(url)
    return seen


def download(url, token, max_redirects=5):
    """GET the matched URL's bytes from its OWN host; return (status, headers,
    content). The token rides the `Authorization: Bearer` header on the first
    (GitHub-owned) hop only and is DROPPED on every redirected hop (GitHub 302s
    the assets shape to signed storage that must not receive it); the User-Agent
    rides every hop. The token never enters a URL or query param, so it is not
    surfaced."""
    send_auth = True
    status, headers, content = None, None, b""
    for _ in range(max_redirects + 1):
        parts = urllib.parse.urlsplit(url)
        request_headers = {"User-Agent": USER_AGENT}
        if send_auth:
            request_headers["Authorization"] = f"Bearer {token}"
        conn = connection(url)
        try:
            target = parts.path or "/"
            if parts.query:
                target = f"{target}?{parts.query}"
            conn.request("GET", target, headers=request_headers)
            response = conn.getresponse()
            status = response.status
            location = response.getheader("Location")
            headers = response.headers
            content = response.read()
        finally:
            conn.close()
        if status in _REDIRECT_STATUSES and location:
            url = urllib.parse.urljoin(url, location)
            send_auth = False  # auth rides only the first, GitHub-owned hop
            continue
        return status, headers, content
    # Redirect budget exhausted: hand back the last (redirect) response so the
    # caller treats it as the non-2xx failure it is.
    return status, headers, content


def derive_extension(headers):
    """Pick a file extension for an extensionless download (the bare-UUID
    `user-attachments/assets/<uuid>` shape carries no filename): the
    Content-Disposition filename's extension first, then a Content-Type→extension
    map, else a safe `.bin` default."""
    disposition = headers.get("Content-Disposition", "") if headers else ""
    match = re.search(r'filename\*?=(?:[^\'"]*\'\')?"?([^\s;"]+)', disposition)
    if match:
        ext = os.path.splitext(os.path.basename(match.group(1)))[1]
        if ext:
            return ext
    content_type = ""
    if headers:
        content_type = headers.get("Content-Type", "").split(";")[0].strip().lower()
    return _CONTENT_TYPE_EXT.get(content_type, ".bin")


def local_name(url, headers):
    """The basename the download is written under: the URL path's last segment
    (basename-only, with the empty/`.`/`..` guard the siblings use), plus a
    derived extension when it has none (the bare-UUID shape) so the Read tool can
    render an image. Returns None for a segment that is not a safe filename."""
    basename = os.path.basename(urllib.parse.urlsplit(url).path)
    if basename in ("", ".", ".."):
        return None
    if not os.path.splitext(basename)[1]:
        basename += derive_extension(headers)
    return basename


def main():
    # --- Credentials + repo: required, from the inherited environment. Name what
    #     is missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITHUB_TOKEN")
    host = os.environ.get("GITHUB_HOST")
    repo = os.environ.get("GITHUB_REPO")
    missing = [
        name
        for name, value in (
            ("GITHUB_TOKEN", token),
            ("GITHUB_HOST", host),
            ("GITHUB_REPO", repo),
        )
        if not value
    ]
    if missing:
        print(
            f"fetch_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = github_api_base(host)

    # --- The issue/PR number: GITHUB_PR_NUMBER, then GITHUB_ISSUE_NUMBER, then
    #     the scratch file fallbacks. ---
    number = issue_number()
    if not number:
        print(
            "fetch_attachment.py: no issue/PR number (set GITHUB_PR_NUMBER or "
            "GITHUB_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
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

    # --- Fetch the issue/PR body, then its comments; a non-2xx on either is a
    #     non-zero exit the agent surfaces. Issues and PRs share the `/issues/{n}`
    #     object, so a PR body is covered too. ---
    status, body = get(api_base, f"/repos/{repo}/issues/{number}", token)
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to read {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)
    issue_body = json.loads(body).get("body", "")

    status, body = get(api_base, f"/repos/{repo}/issues/{number}/comments", token)
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to read comments for {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)
    comment_bodies = [comment.get("body", "") for comment in json.loads(body)]

    # --- Scan the body and every comment for the two embedded-image URL shapes.
    #     No matched URL anywhere is a success, not an error — analogous to the
    #     "no comments" success path — and lands before any `attachments/` dir. ---
    urls = references(issue_body, *comment_bodies)
    if not urls:
        print(f"no attachments on {repo}#{number}")
        sys.exit(0)

    # --- Download each unique URL from its own host and write it under
    #     $AFKD_SCRATCH_DIR/attachments/<token>/<basename>. The `<token>` is a
    #     separator-free hash of the full URL and the `<basename>` is basename-only
    #     with an empty/`.`/`..` guard, so BOTH path segments are traversal-safe. A
    #     non-2xx download is a non-zero exit; the token is never echoed. ---
    for url in urls:
        status, headers, content = download(url, token)
        if not status or not 200 <= status < 300:
            print(
                f"fetch_attachment.py: failed to download {url}",
                file=sys.stderr,
            )
            sys.exit(1)
        basename = local_name(url, headers)
        if basename is None:
            continue
        namespace = hashlib.sha256(url.encode("utf-8")).hexdigest()[:16]
        dest_dir = os.path.join(scratch, "attachments", namespace)
        os.makedirs(dest_dir, exist_ok=True)
        dest = os.path.join(dest_dir, basename)
        with open(dest, "wb") as handle:
            handle.write(content)
        print(dest)


if __name__ == "__main__":
    main()
