# arig

A polyglot service orchestrator. Reads a YAML file describing a set of
services with their commands and dependencies, then builds and runs them
locally with the right startup ordering.

Early development. Only process supervision and dependency ordering work
today. The eventual goal is one tool that runs services from source for
local dev, and emits Kubernetes/Helm manifests for the same config.

## Usage

    arig up

Reads `arig.yaml` in the current directory. Example:

    services:
      db:
        runtime: docker
        image: postgres:16
        ports: ["5432:5432"]
        env:
          POSTGRES_PASSWORD: dev
        ready:
          tcp: 127.0.0.1:5432

      migrate:
        command: ./scripts/migrate.sh
        type: oneshot
        depends_on: [db]

      api:
        build: cargo build
        command: cargo run
        working_dir: ./api
        depends_on: [migrate]

## Commands

`arig up -d` runs the supervisor in the background. It returns once the
supervisor is accepting commands, which is before any service has started.

`arig wait` blocks until every wave is up and every readiness probe has
passed, then exits 0. It exits non-zero, reporting why, if a probe never
passes, a oneshot fails, a service exits while the stack is still coming up,
or the timeout (`--timeout`, default 2m) elapses first.

`arig ps` lists what the supervisor is tracking. `STATUS` reports the process,
`READY` reports the probe: a service is `running` and `pending` between
starting and passing its probe, and `-` when no probe gates it. `DESIRED`
reports what was asked for, so a service stopped on purpose reads differently
from one that died. `RESTARTS`, `UPTIME` and `NOTE` appear when there is
something to say; `NOTE` marks a row whose dependency is no longer running.

`arig down` stops everything.

`arig stop <service>` stops one service and leaves the rest running. It returns
once the service is actually gone.

`arig start <service>` starts one that is not running and returns once its
readiness probe passes, non-zero with the reason if it does not. A probe that
gives up fails the command but leaves the process running. `--no-wait` returns
as soon as it has spawned.

`arig restart <service>` is stop then start, and degrades to a start for a
service that is already stopped. Both it and `start` re-read the service's
definition from the config file, so an edited command or env var takes effect.
An edit to `depends_on` or `type` is refused: the startup order was computed
when the stack came up. A definition that does not validate fails the command
before anything is stopped.

`arig build <service>` runs the service's `build:` command while it keeps
running. `arig restart --build <service>` builds first and leaves the running
instance alone if the build fails.

Lifecycle commands take `--timeout` (default 2m), and are refused while the
stack is still starting. One command per service at a time: while a service is
being built, stopped or spawned, a second command for it is refused rather than
queued. A start that has spawned and is only waiting on its readiness probe is
not holding the service, so a command arriving then takes it over and the
waiting one reports that it was superseded.

## Runtimes

`runtime:` picks what runs a service. It defaults to `process`.

`process` runs `command` through the system shell.

`docker` runs `image` as a container on the local daemon, named
`arig-<service>`. `ports` publishes to the host as `host:container`, or a bare
port for both. `env` becomes the container's environment. `command` overrides
the image's command and is split on whitespace, since a container takes an
argv rather than a shell line. Containers are removed when the service stops,
and one left behind by an earlier run is replaced rather than treated as a
conflict.

## Editor integration

A JSON schema for `arig.yaml` is checked in at `arig.schema.json`. With the
YAML language server installed, add a directive at the top of your config:

    # yaml-language-server: $schema=https://raw.githubusercontent.com/cienvita/arig/main/arig.schema.json

To match the schema to your installed binary instead, generate it locally:

    arig schema > arig.schema.json
    # yaml-language-server: $schema=./arig.schema.json

## Todo

Near-term:
- [x] `-C dir` flag (chdir before reading config)
- [x] Resolve `working_dir` against the yaml file's directory
- [x] TCP health checks with readiness gating
- [ ] HTTP health checks
- [ ] Template rendering for `.arig/templates` -> `.arig/generated`
- [ ] Dynamic env injection from dependency metadata

Command surface:
- [x] Implement `arig down`
- [ ] `--format json` structured output
- [x] Single-service `stop`, `start`, `restart`, `build`, see
      [docs/service-lifecycle.md](docs/service-lifecycle.md)
- [ ] Single-service `status`, `logs`, `env`
- [ ] `arig run` for oneshots, crash-restart policy, watch mode

Plugin platform:
- [x] Docker runtime via bollard
- [ ] Helm/k8s publish plugins
- [ ] External plugin protocol
- [ ] `arig mcp` server

## License

Dual-licensed under either of MIT or Apache-2.0, at your option.
