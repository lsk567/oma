# Team isolation

The team is the unit. Agents in one team share a workspace; agents in different
teams never do.

```toml
# ~/.omar/config.toml
[isolation]
mode = "worktree"                 # none | worktree | container
image = "debian:bookworm-slim"    # container mode only
credential_mounts = ["~/.claude", "~/.codex"]
```

Per run: `omar run team.omar --isolation worktree`.

Applies to deployed teams. The EA's own pane is not isolated — it orchestrates
teams rather than belonging to one.

## Modes

| Mode | Each team gets | Needs |
|---|---|---|
| `none` | nothing; all teams share `[agent] default_workdir` | — |
| `worktree` | a git worktree, on its own branch | a git repo |
| `container` | that worktree, mounted in its own container | docker |

`none` is the default and the previous behaviour.

## worktree

Cut from `[agent] default_workdir`'s repository into the team's own state
directory:

```
~/.omar/ea/0/topologies/alpha/worktree/     branch omar/alpha/<run>
~/.omar/ea/0/topologies/beta/worktree/      branch omar/beta/<run>
```

Every agent in `alpha` launches there. Writes are shared inside the team and
invisible to `beta`.

Removed when a run completes or is stopped. **Kept** when a run fails or is
cancelled — that is when you need to see what the team did. The next deploy of
the same team reclaims the path.

Two limits worth knowing:

- **A worktree is a clean checkout.** No `.env`, no `node_modules`, no build
  cache. Agents that need them will fail until you put them there.
- **It is not a security boundary.** The object database is shared, so a team
  can read another team's branches, and nothing stops an agent walking out of
  the directory. It separates working trees, which buys coordination.

## container

Everything worktree does, plus a docker container per team. Agent panes run
`docker exec` into it, so tmux, `omar list` and attach all work unchanged.

Host paths are mounted at the same absolute paths inside, and `HOME` points at
your host home, so `~/.claude` and the absolute paths OMAR bakes into a launch
command mean the same thing on both sides.

Mounted: the team worktree, `~/.omar`, the `omar` binary, and every existing
path in `credential_mounts`. A path that does not exist is skipped rather than
created.

**The default image carries no agent CLIs.** Container mode probes for each
backend the team uses and refuses to deploy if one is missing, naming it. Point
`image` at something that has them:

```dockerfile
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git ripgrep nodejs npm \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g @anthropic-ai/claude-code @openai/codex opencode-ai
```

```bash
docker build -t omar-agents:latest -f docs/agents.Dockerfile .
```

Then set `image = "omar-agents:latest"`.

### Known limits

- **opencode's side channel does not survive.** OMAR reaches opencode over a
  loopback HTTP port, and the container has its own network namespace, so
  provisioning times out and delivery falls back to typing into the pane. The
  other backends use unix sockets or files under mounted paths and are
  unaffected.
- **Credentials are shared within the team.** Every agent in the container can
  read every mounted login. A team container is a trust boundary, not a
  security one. Narrow `credential_mounts` to what the team actually needs.
- **The tmux server stays on the host.** An agent that can reach OMAR's MCP
  surface can still ask for a pane outside the container. Container mode
  contains what an agent does in its own pane, not what it can ask OMAR for.
