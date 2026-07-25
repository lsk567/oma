# OMAR topology integration corpus

The `.omar` sources live in `tests/topology/src/`. Every executable scenario
except the original heterogeneous `HR.omar` uses Codex for every agent. CI
performs only login-free compiler and Rust VM dry-run checks. Authenticated
end-to-end execution is a local integration suite.

Coverage:

- `AllTypes.omar`: scalar, path/bytes, nested list/option ports; structured values.
- `BusinessLoop.omar`: independently recurring A and B→C subteams,
  offer/ack synchronization at D, and iterative final-delivery QA.
- `FanOutFanIn.omar`: parallel fan-out, tag barrier, and fan-in.
- `OrTriggers.omar`: LF-style OR triggers, different microsteps, absent values.
- `EffectContracts.omar`: alternatives, optional groups, signals, constants.
- `OrderedWrites.omar`: shared-effect ordering, optional writes, last writer wins.
- `Recurrence.omar`: self-triggered action flow across logical microsteps.
- `SameAgentSerial.omar`: same-agent serialization and same-tag fan-in.
- `HRCodex.omar`: the original hiring example using Codex for every role.

To compile and verify one scenario without launching agents:

```sh
cd lang
lake exe omarc ../tests/topology/src/FanOutFanIn.omar /tmp/FanOutFanIn.json
cd ..
cargo run -- topology apply /tmp/FanOutFanIn.json --dry-run
```

To execute it with live Codex agents, replace the final command with
`omar topology run`, provide every declared input, and use `--replace` when
reusing agent names. Live execution requires a configured Codex backend.

To compile and execute the complete Codex-only corpus with representative
inputs and local Codex agents:

```sh
make test-topology
```

Run one case while iterating:

```sh
make test-topology CASE=BusinessLoop
```

The suite intentionally excludes heterogeneous `HR.omar` and runs
`HRCodex.omar` instead. It runs every case even if an earlier case fails. Each
case must compile, exit successfully after completing all effect contracts,
print its completion marker, and produce every declared output expected by the
test.

The runner prints per-case duration and assertion counts, followed by an
aggregate PASS/FAIL verdict. It exits nonzero if any case fails. Logs and a
machine-readable `results.tsv` summary are written under
`target/topology-tests/`.

The following environment variables adjust local execution:

- `OMAR_TEST_CASE_TIMEOUT_SECONDS`: whole-case timeout; default `900`.
- `OMAR_TEST_INVOCATION_TIMEOUT_SECONDS`: timeout for one agent prompt;
  default `300`.
- `OMAR_TEST_CASE`: run only the named case.
- `OMAR_TEST_RESULTS_DIR`: directory for logs and metrics.
