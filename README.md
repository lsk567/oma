<div align="center">

<img src="img/full-black-banner.jpg" alt="OMAR" width="250">

**LLM agents are unpredictable. Their coordination doesn't have to be.**

**`omar` delivers deterministic, formally specified orchestration for multi-agent systems.**

<p align="center">
  <a href="https://omar.rs">omar.rs</a>&nbsp; • &nbsp;
  <a href="https://omar.rs/zh/">中文</a>&nbsp; • &nbsp;
  <a href="https://opensource.org/licenses/BSD-3-Clause"><img src="https://img.shields.io/badge/License-BSD_3--Clause-blue.svg" alt="License" valign="middle"/></a>&nbsp; • &nbsp;
  <a href="https://github.com/lsk567/omar/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/lsk567/omar/ci.yml?label=CI&logo=github" alt="CI Status" valign="middle"/></a>&nbsp; • &nbsp;
  <a href="https://discord.gg/X76PSzmfWr"><img src="https://img.shields.io/discord/1467663881588572182?label=Discord&logo=discord&logoColor=white&color=5865F2&cacheSeconds=60" alt="Discord" valign="middle"/></a>
</p>

<p align="center">
<img src="./img/web.gif" alt="Web UI" valign="middle"/>
Web UI
</p>

<p align="center">
<img src="./img/demo.gif" alt="Terminal UI" valign="middle"/>
Terminal UI
</p>

</div>

## Features

- **Deep hierarchies**: Agents managing agents, just like a company.
- **Heterogeneity**: Let `claude`, `codex`, and other agents collaborate as a team.
- **Full control**: Talk to and control any subagent you want.
- **Life span**: Long-running or ephemeral agents, your choice.
- **Customization**: Support all `tmux` commands you love.

Other features include messaging systems integration (e.g., Slack), computer use, and more.

## Installation

### Prerequisites

- tmux 3.0+
- Rust 1.70+
- GNU Make
- Node.js 22.13+ (to build Mission Control, which `make build` embeds)
- One or more coding agents: [Claude](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/overview), [Codex](https://developers.openai.com/codex/cli), [Cursor](https://cursor.com/cli), [Opencode](https://github.com/anomalyco/opencode), or [Google Antigravity CLI](https://antigravity.google/product/antigravity-cli).

### One-liner (recommended)

```bash
curl -fsSL https://omar.rs/install.sh | sh
```

Installs all binaries to `/usr/local/bin`.

### Homebrew

```bash
brew install omar-os/omar/omar
```

### Build from source

Requires Rust 1.70+ and GNU Make.

```bash
git clone https://github.com/omar-os/omar.git
cd omar && make install
```

## Quick Start

#### Step 1: Launch Mission Control

```bash
$ omar serve --ui
```

Serves the web UI from the daemon's own address and opens it in your browser.

#### Step 2: Describe a workflow

Type what the team should do. The assistant drafts an OMAR program and shows you
the topology it compiles to.
Nothing runs until you press **Confirm deploy**,
then the diagram goes live.

### Terminal UI

The same runtime, driven from tmux instead.

#### Step 1: Launch `omar`

```bash
$ omar
```

Go [here](#supported-agent-backends) to see how to launch with specific agent backends.

#### Step 2: Tell the Executive Assistant (EA) to run a test prompt.

Copy the following into the EA window:
```
Run https://github.com/omar-os/omar/blob/main/prompts/tests/project-factory.md
```

You should see agents being spawned by the EA.

Tip: Use `↑↓←→` to cycle through agents at the current level. Use `Tab` to drill into a deeper level. Use `Shift+Tab` to back out.

#### Step 3: Shutdown the project.

Go back to the EA and type in:
```
Shutdown the test project and its agents.
```

## Working on Mission Control

The web client lives in [`web/`](web/) and is built from this same commit as the
runtime it talks to. `omar serve --ui` hands out a bundle compiled into the
binary; to work on the client itself, use the dev server instead — it reloads:

```bash
make dev
```

Builds the runtime, starts the daemon, starts Mission Control pointed at it,
and opens a browser. Ctrl-C stops both. Needs Node.js 22.13+; see
[`web/README.md`](web/README.md).

## Supported Agent Backends

| Backend | How to launch |
|---------|---------------|
| [Claude Code](https://docs.anthropic.com/en/docs/agents-and-tools/claude-code/overview) | `omar -a claude` (default) |
| [Codex CLI](https://developers.openai.com/codex/cli) | `omar -a codex` |
| [Cursor CLI](https://cursor.com/cli) | `omar -a cursor` |
| [Opencode](https://github.com/anomalyco/opencode) | `omar -a opencode` |
| [Google Antigravity CLI](https://antigravity.google/product/antigravity-cli) | `omar -a agy` |

## License

BSD 3-Clause

## Contributors

Thanks to all of our amazing contributors!

<a href="https://github.com/omar-os/omar/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=omar-os/omar" />
</a>

---

OMAR is made with ❤️ in Berkeley, CA.
