# TODO

Notes from starting arig 0.3.0 non-interactively, where the stack has to
be brought up and then waited on before anything talks to it. All four
below are the same underlying gap: a detached run has no machine-readable
answer to "are the services ready yet".

## `up -d` returns before any service starts

`detach_and_exit` ends with `ipc::wait_ready(&endpoint, 10s)`, which
waits for the supervisor's IPC socket to accept. The flag's help says
"the CLI returns once it is ready", which reads as the stack being ready.
Measured, `arig up -d` returns in about 60ms against a stack whose first
container has not started.

Reword the help at minimum. "returns once the supervisor is accepting"
says what it does without promising the other thing.

## Nothing reports readiness

`ServiceReady` never reaches the state tracker. `state.rs` sets `status`
to `"running"` when a service starts and to the exit string when it ends,
and neither `state.rs` nor `protocol.rs` carries a probe or ready field,
so `arig ps` shows `running` for a service whose probe is still pending.
There is no other query surface, so a caller cannot ask.

Worth a `READY` column fed by the probe result, since `ps` is already the
place people look. A blocking `arig wait` (or `up -d --wait`) would also
close it and is easier to use from a script.

## The only readiness signal is a log line

`arig: N service(s) running.` goes to the event bus and so to
`.arig/var/logs/<ts>/_arig.log`. With nothing else to watch, callers grep
that file, which is fragile in a specific way: every previous run's
`_arig.log` ends with the same line, so grepping through the
`logs/latest` symlink returns true immediately if the symlink has not
been repointed for the new run yet, and the wait passes against a stack
that has not started.

Measured once, the symlink is repointed within 200ms of `up -d`
returning, so this may already be ordered before the socket opens. If it
is, saying so in a comment would let callers rely on it. The workaround
without that guarantee is comparing the symlink target against its
previous value before grepping.

Fixing either of the two items above makes this moot.

## Detached runs print Ctrl+C advice

`supervisor/mod.rs` logs `arig: N service(s) running. Press Ctrl+C to
stop.` unconditionally. Under `up -d` the supervisor has called `setsid`
and has no controlling terminal, so Ctrl+C reaches nothing and `arig
down` is the answer. The line lands in the log file where it is the first
thing a reader sees after startup.
