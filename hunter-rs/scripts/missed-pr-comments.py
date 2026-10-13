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
watermark covers is listed, with the UPDATE that re-raises it: the next
sync then flags `new_comments` and an engage answers it.

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


def run(argv: list[str]) -> str:
    out = subprocess.run(argv, capture_output=True, text=True, check=False)
    if out.returncode != 0:
        raise RuntimeError(f"{' '.join(argv[:3])} failed: {out.stderr.strip()}")
    return out.stdout


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
    """(created_at, author, body) of every comment and review on the PR."""
    host, path = repo_path(url)
    if forge == "gitlab":
        argv = [
            "glab", "api",
            f"projects/{quote(path, safe='')}/merge_requests/{number}/notes?per_page=100&sort=desc",
        ]
        if host != "gitlab.com":
            argv += ["--hostname", host]
        notes = json.loads(run(argv))
        return [
            (n.get("created_at") or n.get("updated_at", ""),
             (n.get("author") or {}).get("username", "?"),
             n.get("body", ""))
            for n in notes
            if not n.get("system")
        ]
    repo = path if host in ("", "github.com") else f"{host}/{path}"
    view = json.loads(
        run(["gh", "pr", "view", str(number), "-R", repo, "--json", "comments,reviews"])
    )
    items = [
        (c.get("createdAt", ""), (c.get("author") or {}).get("login", "?"), c.get("body", ""))
        for c in view.get("comments", [])
    ]
    items += [
        (r.get("submittedAt", ""), (r.get("author") or {}).get("login", "?"), r.get("body", ""))
        for r in view.get("reviews", [])
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
