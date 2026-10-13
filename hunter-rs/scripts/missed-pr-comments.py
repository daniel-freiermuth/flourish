#!/usr/bin/env python3
"""List PR feedback that engage replies before migration 028 buried.

Until then a finished engage that replied stamped the PR's engagement
watermark (`pr_state.last_engaged_activity_at`) with "now + 3 s", so every
comment or review posted while its chain ran -- or sat suspended -- was
taken as answered and never flagged `new_comments`. The worker never saw
those: it read the PR when the chain started.

Hunter posts its reply only after the job finishes, and its replies were
not marked then, so a comment created inside an engage chain's window
(first attempt's `started_at` .. the finishing attempt's `finished_at`) is
not hunter's. Every such comment on a still-open PR that the current
watermark covers, and that no later engage chain read the PR after, is
listed with the UPDATE that re-raises it: the next sync then flags
`new_comments` and an engage answers it.

Read-only: the database is opened `mode=ro` and nothing is changed; review
the list and run the UPDATEs you agree with. Authors are not screened --
a stranger's comment is listed too, and stays ignored by the daemon.

Needs `gh` (GitHub) and/or `glab` (GitLab), authenticated as the daemon is.

Usage: missed-pr-comments.py [path/to/hunter.db]   (default data/hunter.db)
"""

import json
import sqlite3
import subprocess
import sys
from datetime import datetime
from urllib.parse import quote, urlparse

OWN_MARK = "<!-- posted by hunter -->"


def iso_ms(ts: str) -> int:
    return int(datetime.fromisoformat(ts.replace("Z", "+00:00")).timestamp() * 1000)


FORGE_TIMEOUT_S = 120


def run(argv: list[str]) -> str:
    try:
        out = subprocess.run(
            argv, capture_output=True, text=True, check=False, timeout=FORGE_TIMEOUT_S
        )
    except subprocess.TimeoutExpired as err:
        raise RuntimeError(f"{' '.join(argv[:3])} timed out after {FORGE_TIMEOUT_S}s") from err
    if out.returncode != 0:
        raise RuntimeError(f"{' '.join(argv[:3])} failed: {out.stderr.strip()}")
    return out.stdout


def pages(argv: list[str]) -> list[dict]:
    """Every item of a `--paginate`d REST list. Both CLIs print one JSON
    array per page, back to back, so the output is a stream of arrays."""
    text, items, decoder, pos = run(argv), [], json.JSONDecoder(), 0
    while True:
        while pos < len(text) and text[pos].isspace():
            pos += 1
        if pos == len(text):
            return items
        page, pos = decoder.raw_decode(text, pos)
        items.extend(page)


def repo_path(url: str) -> tuple[str, str]:
    """(host, owner/repo path) of an https or scp-style ssh remote."""
    if "://" not in url and ":" in url:
        host, path = url.split("@", 1)[-1].split(":", 1)
    else:
        parsed = urlparse(url)
        host, path = parsed.hostname or "", parsed.path
    path = path.strip("/")
    return host, path.removesuffix(".git")


def feedback(forge: str, url: str, number: int) -> list[tuple[str, str, str]]:
    """(created_at, author, body) of every comment and review on the PR,
    all pages: a missed comment can sit anywhere in a long thread."""
    host, path = repo_path(url)
    project = quote(path, safe="")
    if forge == "gitlab":
        argv = ["glab", "api", "--paginate",
                f"projects/{project}/merge_requests/{number}/notes?per_page=100"]
        if host != "gitlab.com":
            argv += ["--hostname", host]
        return [
            (n.get("created_at") or n.get("updated_at", ""),
             (n.get("author") or {}).get("username", "?"),
             n.get("body") or "")
            for n in pages(argv)
            if not n.get("system")
        ]
    host_flag = [] if host in ("", "github.com") else ["--hostname", host]
    api = ["gh", "api", "--paginate", *host_flag]
    items = [
        (c.get("created_at", ""), (c.get("user") or {}).get("login", "?"), c.get("body") or "")
        for c in pages([*api, f"repos/{path}/issues/{number}/comments?per_page=100"])
    ]
    items += [
        (r.get("submitted_at") or "", (r.get("user") or {}).get("login", "?"), r.get("body") or "")
        for r in pages([*api, f"repos/{path}/pulls/{number}/reviews?per_page=100"])
    ]
    return items


def chain_windows(db: sqlite3.Connection, finding_id: int) -> list[tuple[int, int, int]]:
    """(job id, chain start, finish) of every engage attempt that finished
    `done` -- the only state in which a reply was posted."""
    jobs = {
        row["id"]: row
        for row in db.execute(
            "SELECT id, state, started_at, finished_at, resumed_from FROM jobs "
            "WHERE finding_id = ? AND kind = 'engage'",
            (finding_id,),
        )
    }
    windows = []
    for job in jobs.values():
        if job["state"] != "done" or job["finished_at"] is None:
            continue
        origin, seen = job, set()
        while origin["resumed_from"] in jobs and origin["id"] not in seen:
            seen.add(origin["id"])
            origin = jobs[origin["resumed_from"]]
        windows.append((job["id"], origin["started_at"] or 0, job["finished_at"]))
    return windows


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "data/hunter.db"
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    prs = db.execute(
        "SELECT f.id AS fid, f.pr_url, ps.pr_number, ps.last_engaged_activity_at AS mark, "
        "r.url, r.forge FROM findings f "
        "JOIN pr_state ps ON ps.finding_id = f.id JOIN repos r ON r.id = f.repo_id "
        "WHERE f.status = 'pr_open' AND ps.pr_number IS NOT NULL ORDER BY f.id"
    ).fetchall()
    found = errors = 0
    for pr in prs:
        windows = chain_windows(db, pr["fid"])
        if not windows or pr["mark"] is None:
            continue
        try:
            items = feedback(pr["forge"], pr["url"], pr["pr_number"])
        except (RuntimeError, json.JSONDecodeError) as err:
            print(f"#{pr['fid']} {pr['pr_url']}: cannot read the PR: {err}", file=sys.stderr)
            errors += 1
            continue
        missed = []
        for ts, author, body in items:
            if not ts or body.rstrip().endswith(OWN_MARK):
                continue
            at = iso_ms(ts)
            if at > pr["mark"]:
                continue  # already newer than the watermark: the daemon flags it
            if any(start > at for _, start, _ in windows):
                continue  # a later engage chain read the PR after it, so saw it
            job = next((j for j, start, end in windows if start <= at <= end), None)
            if job is not None:
                missed.append((at, ts, author, body, job))
        if not missed:
            continue
        missed.sort()
        found += len(missed)
        print(f"#{pr['fid']} {pr['pr_url']}")
        for _, ts, author, body, job in missed:
            line = " ".join(body.split())
            print(f"  {ts} {author} (during engage job {job}): {line[:160]}")
        print(
            f"  UPDATE pr_state SET last_engaged_activity_at = {missed[0][0] - 1} "
            f"WHERE finding_id = {pr['fid']};"
        )
    print(f"{found} buried comment(s) on {len(prs)} open PR(s); {errors} PR(s) unreadable",
          file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
