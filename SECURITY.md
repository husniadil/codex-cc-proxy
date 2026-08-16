# Security

## Reporting

Report suspected vulnerabilities privately through GitHub's security advisory
form on this repository rather than in a public issue.

Include what you did, what happened, and what you expected. A proof of concept
helps; a working exploit is not required.

## Posture

**Loopback only.** The daemon binds `127.0.0.1` and refuses any other address.
It performs no authentication, which is safe precisely because every caller
reaching the socket is already a local process running as the user. The address
is not configurable — making it so would remove the assumption the whole posture
rests on.

`ANTHROPIC_AUTH_TOKEN` must be set for the client's sake, and its value is
ignored. It is not a credential and does not protect anything.

**Credentials.** Stored in a file created `0600`, from the outset rather than
tightened afterwards — writing first and adjusting permissions later leaves a
window in which the file is world-readable, and that window is enough. They
never appear in the configuration file, in process arguments, or in logs at any
level. `Debug` is implemented by hand on every type that holds one, and a test
asserts no token appears in its output.

The control socket is owner-only for the same reason: it can clear credentials,
so the filesystem is its access control.

**Refresh tokens.** This proxy runs its own authorization flow and owns its own
refresh-token family. It does not read or write credentials belonging to any
other tool. Families rotate, so sharing one means whichever client refreshes
last invalidates the other.

**No telemetry.** Nothing is collected. Nothing is transmitted anywhere but the
backend the operator authenticated against.

## Scope

The upstream endpoint is not a published or supported API. That it may change or
be withdrawn is a stated limitation rather than a vulnerability.

Reports that the proxy allows a local user to use the local user's own
credentials are not vulnerabilities.
