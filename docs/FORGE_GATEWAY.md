# Forge Gateway

The forge gateway is the provider-neutral source-control boundary for Nulang AI
agents. It lives in `nulang-ai-mcp`, not in the language core.

## Security contract

Agents do not receive forge credentials. A host creates a `ForgeSession` from
trusted identity/policy state and gives the gateway one or more exact-repository
`ForgeGrant` values. Each grant contains an explicit operation set and an
optional Unix expiry.

The first operation vocabulary is:

- `repo.read`
- `branch.create`
- `commit.write`
- `change.create`
- `change.review`
- `change.merge`
- `check.read`

The gateway rejects a command before the provider backend runs unless an
unexpired grant matches both the exact repository and the exact operation.
`commit.write` deliberately does not imply `change.merge`.

Repository wildcards are intentionally absent from v1. Cross-repository agents
receive multiple exact grants instead.

## Provider boundary

`ForgeBackend` consumes provider-neutral `ForgeCommand` values. The initial
`GiteaBackend` maps those commands onto Gitea's REST API.

The Gitea adapter does not own a token, base URL, TLS policy, retry policy, or
credential refresh. Those belong to its injected `HttpTransport`, which is a
trusted host boundary. This keeps secrets out of model-visible tool arguments
and allows Nulang Cloud to substitute short-lived identities later.

Current Gitea mappings:

| Forge command | Gitea endpoint |
| --- | --- |
| Read file | `GET /api/v1/repos/{owner}/{repo}/contents/{path}` |
| Create branch | `POST /api/v1/repos/{owner}/{repo}/branches` |
| Write file | `POST/PUT /api/v1/repos/{owner}/{repo}/contents/{path}` |
| Create change | `POST /api/v1/repos/{owner}/{repo}/pulls` |
| Review change | `POST /api/v1/repos/{owner}/{repo}/pulls/{index}/reviews` |
| Merge change | `POST /api/v1/repos/{owner}/{repo}/pulls/{index}/merge` |
| List checks | `GET /api/v1/repos/{owner}/{repo}/commits/{ref}/statuses` |

The adapter rejects unsafe repository paths before transport, percent-encodes
path/reference components, uses Gitea's base64 content contract, and surfaces
non-2xx responses as backend errors.

## Intended agent roles

A typical engineering swarm should issue different grants:

- planner/reviewer: `repo.read`, `check.read`, optionally `change.review`
- coder: `repo.read`, `branch.create`, `commit.write`, `change.create`
- merge controller: `repo.read`, `check.read`, `change.merge`

The coding model should normally not possess merge authority.

## Next integration steps

1. Bind these commands to MCP tool handlers with the `ForgeSession` supplied
   by the host, never by caller JSON.
2. Add a GitHub backend using the same command contract.
3. Add short-lived credential minting in the Cloud host and keep credentials
   inside the transport implementation.
4. Add Dagger/runner evidence as a separate CI execution interface; forge state
   and CI execution should remain distinct.
