# Agent-safe package inspection

The package manager already supports machine-readable build/test output and the
compiler supports `--check --json`. The read-only `inspect` command supplies
project context without requiring a build or provider credentials.

```sh
nulang nula inspect --json
nulang nula build --json
nulang nula test --json
nulang --check src/main.nula --json
```

The `inspect` response is one JSON object on stdout:

```json
{
  "schema_version": 1,
  "command": "inspect",
  "project": {
    "name": "example",
    "version": "0.1.0",
    "entry": "src/main.nula",
    "capabilities": ["net"],
    "dependencies": ["helper"],
    "lockfile_present": true
  }
}
```

**Security contract:** the command allowlists fields from `Nulang.toml`.
It never prints configured registry URLs, dependency sources, tokens,
credentials, or raw manifest contents. Dependency *names* are reported, not
their connection details. It never resolves dependencies, runs source code,
or contacts any service.

The schema version is independent of the Nulang language version.
Automation should reject unknown schema versions rather than guessing.
Errors produce a nonzero exit status and go to stderr; successful JSON
mode emits no human-readable status lines.
