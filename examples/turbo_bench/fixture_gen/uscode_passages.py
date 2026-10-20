#!/usr/bin/env python3
"""Statute passages of the US Code for turbo_bench's real-embedding fixture (#177).

Downloads the pinned titles of the 2024 Main Edition from govinfo, checks each
file's sha256, and writes one JSON line per passage to OUT:

    {"citation": "17 U.S.C. 107", "text": "17 U.S.C. § 107. Limitations ...\\n..."}

A passage is statute text only: the section heading, then the section's
statutory paragraphs, without source credits or editorial notes. A section
longer than MAX_CHARS is split at paragraph boundaries (and inside a paragraph
only if it alone is too long), and each part repeats the heading; its citation
then names the part, as in "11 U.S.C. 101 (part 2 of 9)". Sections without
statute text (repealed, omitted, transferred) are skipped. Titles come in the
order of TITLES, sections in the order of the title.

US Code text is a work of the US government and in the public domain
(17 U.S.C. 105).

usage: uscode_passages.py OUT [--cache DIR]
"""

import argparse
import hashlib
import json
import re
import sys
import urllib.request
from html.parser import HTMLParser
from pathlib import Path

EDITION = 2024
URL = "https://www.govinfo.gov/content/pkg/USCODE-{edition}-title{title}/html/USCODE-{edition}-title{title}.htm"
# Title, and the sha256 of its HTML file as downloaded on 2026-10-19.
TITLES = [
    (4, "fb659cd80c47fa7d61cd7f2c97b740c2295cd9b63b2cad96acbfe04de129cf9a"),
    (9, "67fb04b2af6d2215ff710fc7bf20882efb93efe102d50a617b852befcaa57bc7"),
    (11, "9ffa6b789edac684679f93cda99b28ccb9e94786aefe80008acf0eb9bcbaca12"),
    (17, "5e42106e930d973551182ccf2b122311aff6a6a42642e269509b14527b250acd"),
    (35, "33417d938bc88cb36125b765e96a87618299c869c6a9b206a7290657f17055de"),
]
MAX_CHARS = 1500

SECTION = re.compile(
    r"<!-- expcite:[^>]*-->\s*"
    r"<!-- field-start:head -->(?P<head>.*?)<!-- field-end:head -->\s*"
    r"<!-- field-start:statute -->(?P<statute>.*?)<!-- field-end:statute -->",
    re.S,
)


class Paragraphs(HTMLParser):
    """The text of each block element, tags stripped."""

    BLOCKS = {"p", "h1", "h2", "h3", "h4", "h5", "td", "div"}

    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.paragraphs = []
        self.current = []

    def handle_starttag(self, tag, attrs):
        if tag in self.BLOCKS:
            self.flush()

    def handle_endtag(self, tag):
        if tag in self.BLOCKS:
            self.flush()

    def handle_data(self, data):
        self.current.append(data)

    def flush(self):
        text = " ".join("".join(self.current).split())
        if text:
            self.paragraphs.append(text)
        self.current = []


def paragraphs(fragment):
    # Page-break comments sit inside paragraphs.
    parser = Paragraphs()
    parser.feed(re.sub(r"<!--.*?-->", "", fragment, flags=re.S))
    parser.close()
    parser.flush()
    return parser.paragraphs


def split_long(paragraph, budget):
    """`paragraph` in about even pieces of at most `budget` chars, cut at
    sentence ends or else spaces."""
    pieces = []
    while len(paragraph) > budget:
        share = -(-len(paragraph) // -(-len(paragraph) // budget))
        window = paragraph[:share]
        cut = max(window.rfind(". "), window.rfind("; "))
        if cut < share // 2:
            cut = window.rfind(" ")
        pieces.append(paragraph[: cut + 1].strip())
        paragraph = paragraph[cut + 1 :].strip()
    pieces.append(paragraph)
    return pieces


def group(pieces, cap):
    """Consecutive pieces, greedily grouped into chunks of at most `cap` chars."""
    out, current = [], []
    for piece in pieces:
        if current and len("\n".join(current + [piece])) > cap:
            out.append(current)
            current = []
        current.append(piece)
    out.append(current)
    return out


def chunks(body, budget):
    """The paragraphs in as few chunks of at most `budget` chars as fit, split
    so the largest chunk is as small as it can be: no stub at the end."""
    pieces = [piece for paragraph in body for piece in split_long(paragraph, budget)]
    count = len(group(pieces, budget))
    low, high = max(map(len, pieces)), budget
    while low < high:
        cap = (low + high) // 2
        if len(group(pieces, cap)) <= count:
            high = cap
        else:
            low = cap + 1
    return group(pieces, low)


def passages(title, page):
    for match in SECTION.finditer(page):
        heading = " ".join(paragraphs(match["head"]))
        number = re.match(r"§\s*([0-9A-Za-z\-–]+)\.", heading)
        body = paragraphs(match["statute"])
        if not number or not body:
            continue
        section = number[1].replace("–", "-")
        heading = f"{title} U.S.C. " + re.sub(r"^§\s*", "§ ", heading)
        parts = chunks(body, MAX_CHARS - len(heading) - 1)
        for i, part in enumerate(parts, 1):
            citation = f"{title} U.S.C. {section}"
            if len(parts) > 1:
                citation += f" (part {i} of {len(parts)})"
            yield {"citation": citation, "text": heading + "\n" + "\n".join(part)}


def fetch(title, sha256, cache):
    path = cache / f"USCODE-{EDITION}-title{title}.htm"
    if not path.exists():
        url = URL.format(edition=EDITION, title=title)
        print(f"uscode_passages: downloading {url}", file=sys.stderr)
        request = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
        with urllib.request.urlopen(request, timeout=300) as response:
            path.write_bytes(response.read())
    data = path.read_bytes()
    actual = hashlib.sha256(data).hexdigest()
    if actual != sha256:
        sys.exit(f"uscode_passages: {path}: sha256 {actual}, pinned {sha256}")
    return data.decode("utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("out", type=Path)
    parser.add_argument("--cache", type=Path, default=Path("target/uscode-2024"))
    args = parser.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)

    lines = []
    for title, sha256 in TITLES:
        found = list(passages(title, fetch(title, sha256, args.cache)))
        print(f"uscode_passages: title {title}: {len(found)} passages", file=sys.stderr)
        lines.extend(json.dumps(p, ensure_ascii=False) for p in found)
    args.out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"uscode_passages: wrote {len(lines)} passages to {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
