# Service lifecycle

Status: draft

## Problem

Once a stack is up, the only control arig offers is `arig down`. A developer
who fixes a bug in one service has to restart the whole stack, which throws
away warm state in every other service (database contents, caches, JIT
warmup) and repays the full startup cost. The common dev loop is: find an
issue, fix it, rebuild one service, restart it, keep everything else running.

The controlling side is not only a human at a terminal. Scripts, CI, and
LLM agents (eventually via `arig mcp`) need the same operations, with
machine-readable results and exit codes that carry a verdict.

## Proposal

Every service gets a lifecycle of `build`, `start`, `stop`, controllable
per service over the existing IPC socket:

    arig stop api
    arig start api
    arig restart api            # stop, start
    arig restart --build api    # build, stop, start
    arig build api              # build only

This changes what the supervisor is. Today it computes waves, starts
everything once, and passively records what happens. Per-service control
requires two model changes, described below: a real per-service state
machine, and a distinction between desired and actual state.

## State machine

Each service instance moves through:

    Stopped -> Building -> Starting -> Running(pending|ready) -> Stopping -> Stopped
                                    \-> Failed

`Building` is skipped for services with no build step. `Running` keeps the
existing readiness split: a service is running and pending between start
and its probe passing. `Failed` covers a build failure, a start failure, or
a probe that never passed. Today `status` is a free-form string in
`ServiceSnapshot` and only moves forward; it becomes this enum.

Lifecycle mutations for a given service are serialized through the kernel.
Two clients issuing `restart api` and `down` concurrently must not race two
starts or interleave a start with a teardown.

## Desired state vs actual state

"api was stopped on purpose" and "api crashed" are different facts that
look identical to the current tracker. The supervisor records what the
operator asked for (up, stopped) separately from what is true. Everything
downstream keys off this distinction:

* a future crash-restart policy restarts a crashed service but leaves a
  deliberately stopped one alone
* `ps` can render "stopped" without it reading as a failure
* a debugger handoff works: stop the supervised api, run it by hand in the
  foreground, `arig start api` later, and nothing fights the stop

This is not a full reconciler loop. The kernel acts on commands and
records intent; convergence machinery can come later without a remodel.

## Build

There is no build phase today; `command: cargo run` conflates build and
start. A new optional per-service key separates them:

    api:
      build: cargo build
      command: cargo run
      ...

Services without `build:` skip the phase. For the docker runtime, `build:`
later maps to an image build context, as in compose; out of scope for the
first cut.

`restart --build` builds before stopping. A failed build leaves the old
instance running and the command exits non-zero, so a broken edit does not
take a working service down.

## Dependents

Stopping `db` while `api` depends on it touches only `db` by default. The
developer knows what they are doing, and this matches the dev-loop intent.
`ps` marks dependents of a stopped or restarting service so the state is
visible rather than silent.

A `--cascade` flag extends the operation downstream: `stop --cascade db`
stops the dependency closure in reverse wave order, `restart --cascade db`
restarts it and everything downstream in wave order. These are the
one_for_one and rest_for_one strategies from Erlang/OTP supervisors.

## Config reload

The dev loop is not only code edits; it is often an edited env var or
command in arig.yaml. A targeted restart re-reads that service's definition
from the config file, so a restart after editing the yaml picks up the
change. Silently restarting with the stale definition from `up` time is a
trap. Structural edits (changed `depends_on`, added or removed services)
are out of scope for restart and are reported as an error telling the user
to bounce the stack.

## Blocking semantics

`arig start` and `arig restart` block until the service's readiness probe
passes and exit non-zero with the reason if it does not, with the same
timeout handling as `arig wait`. Automation wants one call with a verdict,
not a poll loop. `--no-wait` returns after the state transition is
accepted. `arig stop` blocks until the service has stopped, reusing the
existing shutdown hook and timeout machinery.

## Oneshots

`arig run migrate` re-runs a oneshot on demand against the running stack
(schema changed, re-run migrations). Same plumbing, distinct verb, since
"restart" reads wrong for something that is not running.

## Protocol

New `Request` variants: `Stop`, `Start`, `Restart`, `Build`, `Run`, each
carrying a service name and flags. A new CLI against an older detached
supervisor hits the failure mode of issue #40 (opaque parse error on an
unknown op); these verbs ride on whatever fix that issue gets, degrading to
"supervisor too old for this command, restart the stack with the new
binary".

`ServiceSnapshot` grows desired state, a restart counter, and uptime. Log
sinks reattach across restarts; a generation id on the service instance
distinguishes output from consecutive runs.

## Out of scope, enabled later

* watch mode (file changes trigger build+restart): pure composition once
  the verbs exist
* crash-restart policy (`restart: on-failure`, with OTP-style intensity
  limits): same primitives, driven by the supervisor
* partial up (`arig up api` starting a subset plus its dependency
  closure): same DAG walk and verbs
* `arig mcp`: these verbs are the tool surface it would expose

## Sequencing

1. Per-service state machine and desired state in the tracker.
2. `stop`, `start`, `restart` over IPC, blocking until ready.
3. `build:` config key, `arig build`, `restart --build`.
4. `run` for oneshots.
5. Crash policy, watch mode, partial up as separate efforts.

Step 1 builds on the microkernel architecture delivered under issue #29:
the event bus and the `RunningService` stop seam are the primitives these
verbs consume. Per-service lifecycle is the first real consumer of that
architecture beyond `up`/`down`.

## Prior art

* docker compose: per-service `stop`/`start`/`restart`/`build`, `--no-deps`;
  the closest UX match. Its `up` is overloaded; explicit verbs avoid that.
* supervisord: the closest architecture match. Long-lived supervisor,
  per-program state machine (STOPPED/STARTING/RUNNING/BACKOFF/FATAL),
  control socket.
* systemd: per-unit verbs, dependency propagation, `daemon-reload` as the
  explicit config-refresh answer.
* Erlang/OTP: restart strategy vocabulary (one_for_one, rest_for_one) and
  restart intensity limits.
* Kubernetes: desired-state reconciliation, `kubectl rollout restart`.
  Relevant since emitting k8s manifests is arig's stated end goal.
* Tilt, Skaffold, process-compose, overmind: dev-loop tools built around
  rebuilding and restarting one resource at a time.
