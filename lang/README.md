# OMAR language prototype

Build the Lean 4 compiler:

```sh
cd lang
lake build
```

Run a source program directly:

```sh
cargo run -- topology run tests/topology/src/HR.omar \
  --input resume=/absolute/path/to/resume.txt \
  --replace
```

The runtime invokes `omarc` internally and executes the resulting bytecode.
It finds `omarc` beside the `omar` executable, in the source tree's
`lang/.lake/build/bin` directory, or on `PATH`. Set `OMARC_BIN` to use an
explicit compiler path.

Verify a source program without spawning agents:

```sh
cargo run -- topology apply tests/topology/src/HR.omar --dry-run
```

Precompiled bytecode remains supported:

```sh
cd lang
lake exe omarc ../tests/topology/src/HR.omar /tmp/HR.bytecode.json
cd ..
cargo run -- topology apply /tmp/HR.bytecode.json --dry-run
```

The runtime starts topology-scoped agents, delivers enabled prompts at each
logical tag, waits at the global tag barrier, and prints the final outputs.
`--replace` is required when sessions with the same agent names already exist.
