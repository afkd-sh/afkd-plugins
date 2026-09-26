#!/usr/bin/env python3
# list_issues.py — list the project's open issues (ADR-0017 §7, ADR-0041).
#
# A cross-reference helper of the bundled `gitlab` skill: it lists the open issues
# on the project this run works on, via the GitLab REST v4 API, printing one
# `#<iid>  <title>` line per issue — so the agent can find the issue number to
# retarget the comment helpers' `--issue N` override at. It needs no active
# issue/MR number (it lists the whole project), only the credentials and project;
# on a fetch or auth failure it exits non-zero so the agent surfaces the failure
# rather than retrying blindly.
#
# It reads the credentials FROM THE ENVIRONMENT — the env the afkd gitlab trigger
# already merges into the worker child (crates/gitlab/src/common.rs:
# GITLAB_TOKEN / GITLAB_BASE_URL / GITLAB_PROJECT). It does NOT read its own path
# from the env (CLAUDE_PLUGIN_ROOT is not exported into Bash-tool subprocesses —
# ADR-0017 Revision §5); SKILL.md passes the path on the command line, so this
# helper owns only the API mechanics.

import http.client
import json
import os
import sys
import urllib.parse


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


def get_issues(api_base, project, token):
    """GET the project's open issues; return (status, body-bytes). The token rides
    the PRIVATE-TOKEN header, never a query param, so it is not surfaced."""
    conn = connection(api_base)
    try:
        conn.request(
            "GET",
            api_path(api_base, f"/projects/{encode_project(project)}/issues?state=opened"),
            headers={"PRIVATE-TOKEN": token},
        )
        response = conn.getresponse()
        return response.status, response.read()
    finally:
        conn.close()


def render(issue):
    """One line per issue: `#<iid>  <title>`. GitLab exposes the project-local
    number at `iid` (not the global `id`), so `--issue N`/`Closes #N` speak the
    same iid the human sees in the UI."""
    iid = issue.get("iid")
    title = issue.get("title", "")
    return f"#{iid}  {title}"


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
            f"list_issues.py: {' and '.join(missing)} must be set in the environment",
            file=sys.stderr,
        )
        sys.exit(1)
    api_base = gitlab_api_base(base_url)

    # --- Fetch the project's open issues. A non-2xx (e.g. an auth failure) is a
    #     non-zero exit the agent surfaces. ---
    status, body = get_issues(api_base, project, token)
    if not 200 <= status < 300:
        print(
            f"list_issues.py: failed to read issues for {project}",
            file=sys.stderr,
        )
        sys.exit(1)

    # --- GitLab returns a top-level array of issues; a project with no open issues
    #     is success, not an error. ---
    issues = json.loads(body)
    if not issues:
        print(f"no open issues on {project}")
        sys.exit(0)

    print("\n".join(render(i) for i in issues))


if __name__ == "__main__":
    main()
