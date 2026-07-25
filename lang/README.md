# OMAR language prototype

Build the Lean 4 compiler:

```sh
cd lang
lake build
```

Compile a program:

```sh
lake exe omarc ../tests/topology/src/HR.omar /tmp/HR.bytecode.json
```

Verify the bytecode with the Rust VM without spawning agents:

```sh
cargo run -- topology apply /tmp/HR.bytecode.json --dry-run
```

Run the topology with an external input:

```sh
cargo run -- topology run /tmp/HR.bytecode.json \
  --input resume=/absolute/path/to/resume.txt \
  --replace
```

The runtime starts topology-scoped agents, delivers enabled prompts at each
logical tag, waits at the global tag barrier, and prints the final outputs.
`--replace` is required when sessions with the same agent names already exist.
