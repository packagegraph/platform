#!/usr/bin/env python3
"""The one place the graph-corpus discovery block is defined.

Five programs read the published corpus (docs/GRAPH-PUBLICATION.md) and four
of them are inline shell inside Kubernetes manifests, which is how they drifted
apart in the first place: the host rebuild grew a dedup step for duplicate
graph URIs and the Kubernetes readers never did, so the same bucket produced a
deduplicated QLever index and a TDB2 database that unioned both copies.

So there is exactly one copy of the logic -- the region between the
`>>> shared graph corpus discovery >>>` markers in
deploy/quadlet/scripts/qlever-rebuild-index.sh -- and everything else is
generated from it. `extract()` reads it, `shared_block()` renders it for a
YAML embedding, and test_reader_parity.py fails if any reader has diverged.

Run this file directly to print the rendered block.
"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
HOST_SCRIPT = os.path.join(REPO, "deploy/quadlet/scripts/qlever-rebuild-index.sh")

OPEN = "# >>> shared graph corpus discovery >>>"
CLOSE = "# <<< shared graph corpus discovery <<<"
HOST_OPEN = "# >>> host-only >>>"
HOST_CLOSE = "# <<< host-only <<<"

BANNER = (
    "# GENERATED from deploy/quadlet/scripts/qlever-rebuild-index.sh -- edit it\n"
    "# there, not here. test_reader_parity.py fails if these drift apart.\n"
)


def extract(source=None):
    """The shared region of the host script, with the host-only part removed."""
    if source is None:
        with open(HOST_SCRIPT) as fh:
            source = fh.read()

    def cut(text, opener, closer, keep_markers):
        try:
            start = text.index(opener)
            end = text.index(closer)
        except ValueError:
            raise AssertionError(
                f"{opener!r} / {closer!r} missing from the host script -- the "
                f"parity markers were removed or renamed")
        if keep_markers:
            return text[start + len(opener):end].lstrip("\n")
        return text[:start] + text[end + len(closer):]

    region = cut(source, OPEN, CLOSE, keep_markers=True)
    region = cut(region, HOST_OPEN, HOST_CLOSE, keep_markers=False)
    # Removing the host-only region leaves the blank lines that surrounded it.
    while "\n\n\n" in region:
        region = region.replace("\n\n\n", "\n\n")
    return region.strip("\n") + "\n"


def shared_block(indent, source=None):
    """The block as it appears inside a YAML `args:` literal, indented."""
    pad = " " * indent
    body = BANNER + extract(source)
    return "".join(pad + line if line.strip() else "\n"
                   for line in body.splitlines(keepends=True))


if __name__ == "__main__":
    sys.stdout.write(shared_block(int(sys.argv[1]) if len(sys.argv) > 1 else 0))
