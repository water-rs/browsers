#!/usr/bin/env python3
"""Compare the pinned WPE WebKit version with wpewebkit.org and report drift.

Reads `version` from `browser-wpe/runtime/source.toml`, fetches the release
index <https://wpewebkit.org/release/> and the security index
<https://wpewebkit.org/security/>, then either prints the issue it would file
(--dry-run) or opens/updates one GitHub issue through `gh`.

The site publishes Atom feeds (feed.xml, security.xml), but they carry only
the ten most recent posts of every kind mixed together — releases or
advisories that age past that window would silently vanish from the
comparison, which is exactly the drift this script exists to catch. The HTML
indices list everything, and the release index marks each entry `stable` or
`unstable` itself (odd minors such as 2.53.x are the development snapshots),
so they are the source of truth here.

Every page is parsed against the structure the site serves today; a page that
no longer matches raises instead of reporting "nothing new".
"""

import argparse
import concurrent.futures
import datetime
import html.parser
import json
import pathlib
import re
import string
import subprocess
import sys
import urllib.error
import urllib.request
from html import unescape

GITHUB_DIR = pathlib.Path(__file__).resolve().parent
SOURCE_TOML = GITHUB_DIR.parent / "browser-wpe" / "runtime" / "source.toml"
ISSUE_TEMPLATE = GITHUB_DIR / "wpe-release-watch-issue.md"

SITE = "https://wpewebkit.org"
RELEASE_INDEX = SITE + "/release/"
SECURITY_INDEX = SITE + "/security/"

TITLE_PREFIX = "wpe: WPE WebKit "
USER_AGENT = "water-rs/browsers wpe-release-watch"
ADVISORY_CONCURRENCY = 8
FETCH_TIMEOUT_SECONDS = 30

VERSION = re.compile(r"(\d+)\.(\d+)\.(\d+)")
RELEASE_HREF = re.compile(r"/release/wpewebkit-(\d+\.\d+\.\d+)\.html")
ADVISORY_HREF = re.compile(r"/security/(WSA-\d{4}-\d{4})\.html")
ADVISORY_PAGE = re.compile(r"/security/\d+/")
VERSIONS_AFFECTED = re.compile(r"WPE WebKit before (\d+\.\d+\.\d+)")


class WatchError(Exception):
    """A failed check or a page that no longer parses."""


class IndexParser(html.parser.HTMLParser):
    """Collects every `<li>` with its classes, anchor hrefs and <time> text."""

    def __init__(self):
        super().__init__()
        self.items = []
        self._item = None
        self._in_time = False

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "li":
            if self._item is not None:
                raise WatchError("index page nests <li> elements")
            self._item = {
                "classes": attrs.get("class", "").split(),
                "hrefs": [],
                "date": "",
            }
        elif self._item is not None and tag == "a" and "href" in attrs:
            self._item["hrefs"].append(attrs["href"])
        elif self._item is not None and tag == "time":
            self._in_time = True

    def handle_data(self, data):
        if self._in_time and self._item is not None:
            self._item["date"] += data

    def handle_endtag(self, tag):
        if tag == "time":
            self._in_time = False
        elif tag == "li":
            if self._item is None:
                raise WatchError("index page closes an <li> it never opened")
            self.items.append(self._item)
            self._item = None

    def close(self):
        super().close()
        if self._item is not None:
            raise WatchError("index page ends inside an <li>")


def parse_version(text):
    match = VERSION.fullmatch(text.strip())
    if match is None:
        raise WatchError(f"malformed version {text!r}")
    return int(match[1]), int(match[2]), int(match[3])


def format_version(version):
    return ".".join(str(part) for part in version)


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
            return response.read().decode("utf-8")
    except (urllib.error.URLError, TimeoutError) as error:
        raise WatchError(f"fetching {url}: {error}") from error


def read_pin():
    matches = re.findall(r'(?m)^version\s*=\s*"([^"]+)"\s*$', SOURCE_TOML.read_text())
    if len(matches) != 1:
        raise WatchError(
            f"{SOURCE_TOML}: expected exactly one version line, found {len(matches)}"
        )
    return parse_version(matches[0])


def release_date(text, version):
    try:
        return datetime.datetime.strptime(text.strip(), "%a %b %d %Y").date().isoformat()
    except ValueError:
        raise WatchError(f"release {version}: unparseable release date {text.strip()!r}")


def stable_releases(index_html):
    parser = IndexParser()
    parser.feed(index_html)
    parser.close()
    releases = {}
    for item in parser.items:
        hrefs = [h for h in item["hrefs"] if h.startswith("/release/wpewebkit-")]
        if not hrefs:
            continue
        if len(hrefs) != 1:
            raise WatchError(
                f"release index puts {len(hrefs)} wpewebkit links in one entry"
            )
        match = RELEASE_HREF.fullmatch(hrefs[0])
        if match is None:
            raise WatchError(f"release index links unexpected href {hrefs[0]!r}")
        classes = set(item["classes"])
        if "unstable" in classes:
            continue
        if "stable" not in classes:
            raise WatchError(
                f"release {match[1]} is marked neither stable nor unstable: {sorted(classes)}"
            )
        version = parse_version(match[1])
        releases[version] = release_date(item["date"], match[1])
    if not releases:
        raise WatchError(
            "release index lists no stable wpewebkit releases — page structure changed"
        )
    return releases


def security_advisories(index_html):
    # The index paginates: `/security/` holds the newest entries and links
    # `/security/1/`, `/security/2/` … for the older ones. Follow them all —
    # an advisory affecting the pin is not necessarily recent.
    pages = {"/security/": index_html}
    queue = ["/security/"]
    advisories = {}
    while queue:
        path = queue.pop()
        parser = IndexParser()
        parser.feed(pages[path])
        parser.close()
        for item in parser.items:
            for href in item["hrefs"]:
                if not href.startswith("/security/") or href == "/security/":
                    continue
                match = ADVISORY_HREF.fullmatch(href)
                if match is not None:
                    date = item["date"].strip()
                    try:
                        datetime.date.fromisoformat(date)
                    except ValueError:
                        raise WatchError(
                            f"advisory {match[1]}: unparseable date {date!r}"
                        )
                    advisories[match[1]] = (date, SITE + href)
                elif ADVISORY_PAGE.fullmatch(href):
                    if href not in pages:
                        pages[href] = fetch(SITE + href)
                        queue.append(href)
                else:
                    raise WatchError(
                        f"security index links unexpected href {href!r}"
                    )
    if not advisories:
        raise WatchError(
            "security index lists no advisories — page structure changed"
        )
    return [
        (wsa_id, date, url)
        for wsa_id, (date, url) in sorted(advisories.items())
    ]


def advisory_versions(wsa_id, url):
    text = re.sub(r"<[^>]+>", " ", fetch(url))
    text = re.sub(r"\s+", " ", unescape(text))
    if "Versions affected" not in text:
        raise WatchError(f"{wsa_id}: no 'Versions affected' line — page structure changed")
    versions = [parse_version(v) for v in VERSIONS_AFFECTED.findall(text)]
    if not versions:
        raise WatchError(
            f"{wsa_id}: 'Versions affected' lines carry no WPE WebKit version"
        )
    return versions


def gh(*args):
    try:
        result = subprocess.run(
            ["gh", *args], capture_output=True, text=True, timeout=60
        )
    except subprocess.TimeoutExpired:
        raise WatchError(f"gh {' '.join(args)} timed out")
    if result.returncode != 0:
        raise WatchError(f"gh {' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def publish(title, labels, body):
    listing = gh(
        "issue", "list", "--state", "open", "--limit", "200",
        "--json", "number,title,labels",
    )
    matches = [
        issue for issue in json.loads(listing)
        if issue["title"].startswith(TITLE_PREFIX)
    ]
    if len(matches) > 1:
        numbers = ", ".join(f"#{issue['number']}" for issue in matches)
        raise WatchError(
            f"{len(matches)} open issues start with {TITLE_PREFIX!r}: {numbers}"
        )
    if not matches:
        args = ["issue", "create", "--title", title, "--body", body]
        for label in sorted(labels):
            args += ["--label", label]
        print(gh(*args).strip())
        return
    issue = matches[0]
    current = {label["name"] for label in issue["labels"]}
    args = [
        "issue", "edit", str(issue["number"]), "--title", title, "--body", body,
    ]
    for label in sorted(labels - current):
        args += ["--add-label", label]
    for label in sorted((current - labels) & {"bug"}):
        args += ["--remove-label", label]
    gh(*args)
    print(f"updated issue #{issue['number']}: {title}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--pin",
        help="compare against this version instead of the one pinned in source.toml",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the would-be issue instead of opening or updating one",
    )
    arguments = parser.parse_args()

    pin = parse_version(arguments.pin) if arguments.pin else read_pin()
    releases = stable_releases(fetch(RELEASE_INDEX))
    index = security_advisories(fetch(SECURITY_INDEX))
    with concurrent.futures.ThreadPoolExecutor(
        max_workers=ADVISORY_CONCURRENCY
    ) as pool:
        futures = [
            pool.submit(advisory_versions, wsa_id, url)
            for wsa_id, _date, url in index
        ]
        affected = []
        for (wsa_id, date, url), future in zip(index, futures):
            remaining = [v for v in future.result() if v > pin]
            if remaining:
                affected.append((wsa_id, date, url, len(remaining), max(remaining)))

    newer = sorted((v for v in releases if v > pin), reverse=True)
    affected.sort(key=lambda entry: entry[0], reverse=True)
    print(
        f"pin {format_version(pin)}: {len(newer)} newer stable releases, "
        f"{len(affected)} advisories affect it",
        file=sys.stderr,
    )
    if not newer and not affected:
        return

    newest = max([*newer, *(entry[4] for entry in affected)])
    title = f"{TITLE_PREFIX}{format_version(newest)} is available"
    labels = {"area:wpe"} | ({"bug"} if affected else set())
    body = string.Template(ISSUE_TEMPLATE.read_text()).substitute(
        pinned=format_version(pin),
        releases="\n".join(
            f"- [{format_version(v)}]({SITE}/release/wpewebkit-{format_version(v)}.html)"
            f" — released {releases[v]}"
            for v in newer
        ) or "- None.",
        advisories="\n".join(
            f"- [{wsa_id}]({url}) — {date}; "
            f"{count} vulnerabilities fixed in {format_version(fixed)}"
            for wsa_id, date, url, count, fixed in affected
        ) or "- None.",
        check_date=datetime.date.today().isoformat(),
    )
    if arguments.dry_run:
        print(f"title: {title}\nlabels: {', '.join(sorted(labels))}\n\n{body}")
        return
    publish(title, labels, body)


if __name__ == "__main__":
    try:
        main()
    except WatchError as error:
        sys.exit(f"wpe-release-watch: {error}")