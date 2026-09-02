# Example image for `[isolation] mode = "container"`.
#
# Container isolation runs each agent inside the image, so the image must carry
# every backend the team uses. Adjust the install list to your backends and
# pin versions for a reproducible team.
#
#   docker build -t omar-agents:latest -f docs/agents.Dockerfile .
#   # then set image = "omar-agents:latest" under [isolation]
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        ripgrep \
        nodejs \
        npm \
    && rm -rf /var/lib/apt/lists/*

# claude, codex, opencode. cursor and agy install through their own scripts;
# see each vendor's docs.
RUN npm install -g \
        @anthropic-ai/claude-code \
        @openai/codex \
        opencode-ai

# OMAR mounts the worktree, ~/.omar, the omar binary and your credential dirs
# at their host paths and sets HOME to your host home, so nothing here needs to
# know where they live.
CMD ["sleep", "infinity"]
