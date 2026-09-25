#!/usr/bin/env python3
# fetch_attachment.py — download one Gitea issue/PR asset to scratch (ADR-0017 §7, ADR-0031).
#
# The fetch half of the bundled `gitea` skill's attachment surface (listing is
# list_attachments.py): given an asset id it downloads that single asset into
# `$AFKD_SCRATCH_DIR/attachments/` and prints the written path, so the agent can
# then `Read` the file (e.g. see a screenshot a human attached to the issue). The
# asset id is a REQUIRED positional argument; everything else defaults from the
# environment. On any failure it exits non-zero so the agent surfaces the failure
# rather than retrying blindly.
#
# RESOLVING A BARE ASSET ID. An id from list_attachments.py may belong to the
# issue/PR OR to one of its comments, and the two live under different endpoints.
# So fetch RE-ENUMERATES both (the same collection list_attachments.py does) and
# finds the asset whose id matches, then downloads its `browser_download_url`. This
# resolves either origin uniformly without assuming a comment-asset-by-id route.
#
# SINGLE AUTH SHAPE. Unlike Trello (a separate download host that rejects header-
# less query creds), Gitea's asset download is assumed same-host with the same
# `token` Authorization header as every other call — no query-vs-header split. A
# relative `browser_download_url` is joined to GITEA_BASE_URL; an absolute one is
# used as returned, still with the token header. If a deployment breaks that
# assumption the download fails loudly (non-2xx → non-zero), never silently.
#
# It reads the issue/PR number and credentials FROM THE ENVIRONMENT — the env the
# afkd gitea trigger already merges into the worker child (crates/gitea/src/common.rs:
# GITEA_TOKEN / GITEA_BASE_URL / GITEA_REPO / GITEA_PR_NUMBER / GITEA_ISSUE_NUMBER,
# and AFKD_SCRATCH_DIR holding the per-run scratch dir). It does NOT read its own
# path from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses
# — ADR-0017 Revision §5); SKILL.md passes the path on the command line. It reads
# GITEA_BASE_URL directly — the trigger tests point it at a loopback host, so no
# separate base override is needed.

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


def issue_number():
    """The issue/PR number: GITEA_PR_NUMBER, then GITEA_ISSUE_NUMBER, then the
    <scratch>/pr/number → <scratch>/issue/number file fallbacks, else None."""
    number = os.environ.get("GITEA_PR_NUMBER") or os.environ.get("GITEA_ISSUE_NUMBER")
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


def get_json(base, path, token):
    """GET `path`; return (status, body-bytes). The token rides the Authorization
    header, never a query param, so it is not surfaced."""
    conn = connection(base)
    try:
        conn.request("GET", path, headers={"Authorization": f"token {token}"})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def collect_assets(base, repo, number, token):
    """Return a flat list of the issue/PR's assets AND each of its comments' assets.

    Identical collection to list_attachments.py so a fetch resolves any id that the
    listing printed, whichever container it came from. A non-2xx on the issue-assets
    or the comments call raises RuntimeError; a comment whose own assets call fails
    is skipped rather than aborting the whole enumeration.
    """
    assets = []

    status, body = get_json(base, f"/api/v1/repos/{repo}/issues/{number}/assets", token)
    if not 200 <= status < 300:
        raise RuntimeError(f"failed to read assets for {repo}#{number}")
    assets.extend(json.loads(body))

    status, body = get_json(
        base, f"/api/v1/repos/{repo}/issues/{number}/comments", token
    )
    if not 200 <= status < 300:
        raise RuntimeError(f"failed to read comments for {repo}#{number}")
    for comment in json.loads(body):
        comment_id = comment.get("id")
        if comment_id is None:
            continue
        status, body = get_json(
            base, f"/api/v1/repos/{repo}/issues/comments/{comment_id}/assets", token
        )
        if 200 <= status < 300:
            assets.extend(json.loads(body))

    return assets


def download(url, base, token):
    """GET the asset `url` with the token header; return (status, bytes).

    A relative url is joined to `base` (GITEA_BASE_URL); an absolute one is used as
    returned. The same `token` Authorization header rides the download as every
    other call (single auth shape). The token is never echoed.
    """
    resolved = urllib.parse.urljoin(base, url)
    parts = urllib.parse.urlsplit(resolved)
    target = parts.path
    if parts.query:
        target += f"?{parts.query}"
    if parts.scheme == "https":
        conn = http.client.HTTPSConnection(parts.hostname, parts.port)
    else:
        conn = http.client.HTTPConnection(parts.hostname, parts.port)
    try:
        conn.request("GET", target, headers={"Authorization": f"token {token}"})
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    # The asset id is required (no env default): argparse emits its own error
    # (exit 2) when it is missing, before any network.
    parser.add_argument("asset_id", help="the asset id (from list_attachments.py)")
    args = parser.parse_args()

    # --- Credentials + repo: required, from the inherited environment. Name what
    #     is missing so the config author knows which var to wire (ADR-0023). ---
    token = os.environ.get("GITEA_TOKEN")
    base = os.environ.get("GITEA_BASE_URL")
    repo = os.environ.get("GITEA_REPO")
    missing = [
        name
        for name, value in (
            ("GITEA_TOKEN", token),
            ("GITEA_BASE_URL", base),
            ("GITEA_REPO", repo),
        )
        if not value
    ]
    if missing:
        print(
            f"fetch_attachment.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- The issue/PR number: GITEA_PR_NUMBER, then GITEA_ISSUE_NUMBER, then
    #     the scratch file fallbacks. ---
    number = issue_number()
    if not number:
        print(
            "fetch_attachment.py: no issue/PR number (set GITEA_PR_NUMBER or "
            "GITEA_ISSUE_NUMBER, or AFKD_SCRATCH_DIR/{pr,issue}/number)",
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

    # --- Enumerate the issue's and its comments' assets, then locate the one whose
    #     id matches. A non-2xx during enumeration is a non-zero exit. ---
    try:
        assets = collect_assets(base, repo, number, token)
    except RuntimeError as err:
        print(f"fetch_attachment.py: {err}", file=sys.stderr)
        sys.exit(1)
    asset = next(
        (a for a in assets if str(a.get("id")) == args.asset_id),
        None,
    )
    if asset is None:
        print(
            f"fetch_attachment.py: no asset {args.asset_id} on {repo}#{number}",
            file=sys.stderr,
        )
        sys.exit(1)
    url = asset.get("browser_download_url")
    name = asset.get("name")
    if not url or not name:
        print(
            f"fetch_attachment.py: asset {args.asset_id} has no downloadable url",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Download the bytes with the token header. A non-2xx is a non-zero exit the
    #     agent surfaces; the token is never echoed. ---
    status, content = download(url, base, token)
    if not 200 <= status < 300:
        print(
            f"fetch_attachment.py: failed to download asset {args.asset_id}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- Write into $AFKD_SCRATCH_DIR/attachments/, using the asset's own name
    #     (basename only, so a crafted name can't escape the directory). ---
    dest_dir = os.path.join(scratch, "attachments")
    os.makedirs(dest_dir, exist_ok=True)
    dest = os.path.join(dest_dir, os.path.basename(name))
    with open(dest, "wb") as handle:
        handle.write(content)

    print(dest)


if __name__ == "__main__":
    main()
