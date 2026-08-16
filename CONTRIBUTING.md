# Contributing

## The gate

```sh
just check
```

Formatting, `clippy -D warnings`, and the whole suite. It is what CI runs, and
it is the same command locally — a gate that differs between the two is a gate
that gets argued with.

## The specification comes first

[`docs/proxy-behavior.md`](docs/proxy-behavior.md) is normative. Read the
relevant section before touching translation, transport, sessions, or token
accounting.

Most of its rules exist because the obvious implementation is wrong in a way
that does not fail loudly, and each such rule says so. If implementation shows a
rule is wrong, **change the spec in the same commit as the code that proved it**.
A spec that drifts from the code is worse than no spec, because it is still
believed.

## Development is test-first

Failing test, then the code that passes it, then refactor.

For translation this is straightforward: every rule is a pure function over
data, and the expected output is a specification.

For anything about upstream behaviour it is not guesswork either — record a real
exchange, make the recording a fixture, write the failing test against the
fixture, then implement. `just record ingress` captures what the client sends
and costs nothing.

## Tests that cannot fail are worse than no tests

Before adding a test that passes on the first run, make it fail. Break the code
it covers, watch it go red, and put the code back. Several tests in this project
were written, passed immediately, and turned out to assert nothing — a
cancellation test whose timing made the assertion unreachable, a transport
comparison where both sides computed the same value. Each was caught only by
deliberately breaking the thing it claimed to check.

The specific traps, all of which have already happened here:

- A timing assertion whose window makes the failure impossible.
- A comparison where both sides are derived from the same source.
- An expected value recomputed the way the code computes it.
- A probe keyed on something the model could infer without the evidence.

## No test touches the network

Every upstream interaction runs against a local replay server. This is a design
constraint, not a convenience: a test that needs a live backend is a test that
stops running the moment quota runs out.

`just doctor` and `just record upstream` are the only things that spend quota,
and neither is part of the gate.

## Say what is derived and what is confirmed

A capability verified against replayed fixtures is derived, not confirmed. Where
something can only be settled by a live backend, add it to
[`docs/roadmap.md`](docs/roadmap.md) §L rather than guessing — and never write
output that reads like confirmation when it is not.

## Naming

Identifiers describe what they do, not who calls them. The upstream is a
provider, not a brand; the client is a harness, not a product tier. Comments may
name a real client or endpoint where that is the accurate explanation for a rule
— the constraint is on names, not on prose that has to be true to be useful.

## Commits

Small, working increments, each reviewable and revertible on its own. The commit
message should say why, not what; the diff already says what.
