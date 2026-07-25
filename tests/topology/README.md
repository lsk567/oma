# OMAR topology integration corpus

The `.omar` sources live in `tests/topology/src/`. Every scenario except the
heterogeneous `HR.omar` uses Codex for every agent. CI performs login-free
compiler and Rust VM dry-run checks on every pull request. The authenticated
suite runs from trusted branches and can also be invoked locally.

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
- `SuperdenseTime.omar`: `(timestamp, microstep)` tags, fixed action delay,
  additive connection delay, and chronological delivery.
- `HR.omar`: the heterogeneous hiring example using Claude and Codex.
- `HRCodex.omar`: the original hiring example using Codex for every role.

To compile and verify one scenario without launching agents:

```sh
cd lang
lake exe omarc ../tests/topology/src/FanOutFanIn.omar /tmp/FanOutFanIn.json
cd ..
cargo run -- topology apply /tmp/FanOutFanIn.json --dry-run
```

To execute it with live agents, replace the final command with
`omar topology run`, provide every declared input, and use `--replace` when
reusing agent names. Live execution requires every backend declared by that
topology to be installed and authenticated.

To compile and execute the complete corpus with representative inputs and local
agents:

```sh
make test-topology
```

Run one case while iterating:

```sh
make test-topology CASE=BusinessLoop
```

The suite includes both heterogeneous `HR.omar` and Codex-only `HRCodex.omar`.
It runs every case even if an earlier case fails. Each case must compile, exit
successfully after completing all effect contracts, print its completion marker,
and produce every declared output expected by the test.

The `Topology CI Test` GitHub Actions workflow runs the authenticated suite on
trusted changes to topology code on `main`, on a weekly schedule, or by manual
dispatch from `main` or the trusted `omar-lang` integration branch. It expects
`OPENAI_API_KEY` and `ANTHROPIC_API_KEY` repository secrets. Secrets are not
made available to the ordinary pull-request checks.

The runner prints per-case duration and assertion counts, followed by an
aggregate PASS/FAIL verdict. It exits nonzero if any case fails. Logs and a
machine-readable `results.tsv` summary are written under
`target/topology-tests/`.

The following environment variables adjust local execution:

- `OMAR_TEST_CASE_TIMEOUT_SECONDS`: whole-case timeout; default `900`.
- `OMAR_TEST_INVOCATION_TIMEOUT_SECONDS`: timeout for one agent prompt;
  default `300`.
- `OMAR_TEST_CASE`: run only the named case.
- `OMAR_TEST_PEEK_AGENTS`: capture recent tmux output from every live agent once
  per minute; enabled by the CI workflow.
- `OMAR_TEST_RESULTS_DIR`: directory for logs and metrics.
