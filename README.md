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
        command: cargo run
        working_dir: ./api
        depends_on: [migrate]

## Commands

`arig up -d` runs the supervisor in the background. It returns once the
supervisor is accepting commands, which is before any service has started.

`arig wait` blocks until every wave is up and every readiness probe has
passed, then exits 0. It exits non-zero if the supervisor gives up on a probe
or the timeout (`--timeout`, default 2m) elapses first.

`arig ps` lists what the supervisor is tracking. `STATUS` reports the process,
`READY` reports the probe: a service is `running` and `pending` between
starting and passing its probe, and `-` when no probe gates it.

`arig down` stops everything.

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
- [ ] Implement `arig down`
- [ ] `--format json` structured output
- [ ] Single-service commands (`status`, `logs`, `env`, `restart`, `build`)

Plugin platform:
- [x] Docker runtime via bollard
- [ ] Helm/k8s publish plugins
- [ ] External plugin protocol
- [ ] `arig mcp` server

## License

Dual-licensed under either of MIT or Apache-2.0, at your option.
