#!/usr/bin/env python3
"""Keep Ubuntu amd64 mirrors amd64-only before adding ARM64 ports to CI apt.

Ubuntu 22.04 runners may use sources.list *and* deb822 ubuntu.sources at once.
Changing only one leaves apt querying arm64 on security.ubuntu.com (404).
"""
import argparse
from pathlib import Path
import re
from urllib.parse import urlsplit


def is_ubuntu_main_mirror(value: str) -> bool:
    parsed = urlsplit(value)
    host = parsed.hostname or ""
    return (
        parsed.scheme in ("http", "https")
        and (host in ("archive.ubuntu.com", "security.ubuntu.com")
             or host.endswith(".archive.ubuntu.com"))
        and parsed.path.rstrip("/") == "/ubuntu"
    )


def restrict_list(text: str) -> str:
    """Restrict official Ubuntu one-line sources, preserving unrelated repos."""
    result = []
    for line in text.splitlines(keepends=True):
        match = re.match(r"^(\s*deb(?:-src)?\s+)(?:\[([^]\n]+)\]\s+)?(\S+)([^\n]*)(\n?)$", line)
        if match is None or not is_ubuntu_main_mirror(match.group(3)):
            result.append(line)
            continue
        options = [part for part in (match.group(2) or "").split() if not part.startswith("arch=")]
        options.append("arch=amd64")
        result.append(f"{match.group(1)}[{' '.join(options)}] {match.group(3)}{match.group(4)}{match.group(5)}")
    return "".join(result)


def restrict_deb822(text: str) -> str:
    """Constrain only official Ubuntu deb822 stanzas to amd64."""
    chunks = re.split(r"(\n\s*\n)", text)
    for n in range(0, len(chunks), 2):
        part = chunks[n]
        uri = re.search(r"(?m)^URIs:\s*(.*)$", part)
        if not uri or not any(is_ubuntu_main_mirror(u) for u in uri.group(1).split()):
            continue
        if re.search(r"(?m)^Architectures:\s*", part):
            part = re.sub(r"(?m)^Architectures:[^\n]*", "Architectures: amd64", part)
        else:
            part, count = re.subn(r"(?m)^(Types:[^\n]*\n)", r"\1Architectures: amd64\n", part, count=1)
            if not count:
                raise ValueError("official Ubuntu deb822 source lacks Types field")
        chunks[n] = part
    return "".join(chunks)


def configure(apt: Path, codename: str) -> None:
    if not re.fullmatch(r"[a-z][a-z0-9-]*", codename):
        raise ValueError(f"invalid Ubuntu codename: {codename!r}")
    changed = 0
    candidates = [
        apt / "sources.list",
        *sorted((apt / "sources.list.d").glob("*.list")),
        *sorted((apt / "sources.list.d").glob("*.sources")),
    ]
    for path in candidates:
        if not path.is_file() or path.name == "nulang-arm64.sources":
            continue
        original = path.read_text(encoding="utf-8")
        processed = restrict_deb822(original) if path.suffix == ".sources" else restrict_list(original)
        if original != processed:
            path.write_text(processed, encoding="utf-8")
            changed += 1

    (apt / "sources.list.d").mkdir(parents=True, exist_ok=True)
    (apt / "sources.list.d/nulang-arm64.sources").write_text(
        "Types: deb\n"
        "URIs: http://ports.ubuntu.com/ubuntu-ports/\n"
        f"Suites: {codename} {codename}-updates {codename}-backports {codename}-security\n"
        "Components: main restricted universe multiverse\n"
        "Architectures: arm64\n"
        "Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n",
        encoding="utf-8",
    )
    print(f"Configured Ubuntu apt ARM64 ports; amended {changed} source file(s)")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apt-dir", type=Path, default=Path("/etc/apt"))
    parser.add_argument("--codename", required=True)
    args = parser.parse_args()
    configure(args.apt_dir, args.codename)


if __name__ == "__main__":
    main()
