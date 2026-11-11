"""Safe CLI argument construction for executable conformance fixtures.

Resource grants are denied by default and must be explicitly declared in
checked-in fixture JSON. Never pass arbitrary command-line fragments through.
"""

_ALLOWED_CAPABILITIES = frozenset({"fs", "net", "os"})


def build_command(binary: str, filename: str, expected: dict) -> list[str]:
    """Build a Nulang argv with exact, allowlisted fixture-specific grants."""
    capabilities = expected.get("capabilities", [])
    if not isinstance(capabilities, list):
        raise ValueError("fixture capabilities must be a list of fs/net/os")
    if (
        any(not isinstance(cap, str) or cap not in _ALLOWED_CAPABILITIES for cap in capabilities)
        or len(capabilities) != len(set(capabilities))
    ):
        raise ValueError("fixture capabilities must be unique and among fs/net/os")
    if not capabilities:
        return [binary, filename]
    return [binary, "--with", ",".join(capabilities), filename]


def select_fixture_names(available: list[str], only: list[str] | None) -> list[str]:
    """Return exact fixture names in stable order without silently omitting typos."""
    names = sorted(set(available))
    if only is None:
        return names
    if not only:
        raise ValueError("the explicit fixture selection must not be empty")
    if len(only) != len(set(only)):
        raise ValueError("fixture selection contains duplicate names")
    missing = sorted(set(only) - set(names))
    if missing:
        raise ValueError("unknown conformance fixtures: " + ", ".join(missing))
    return sorted(only)
