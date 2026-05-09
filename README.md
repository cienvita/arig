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
        command: docker run --rm -p 5432:5432 -e POSTGRES_PASSWORD=dev postgres:16

      migrate:
        command: ./scripts/migrate.sh
        type: oneshot
        depends_on: [db]

      api:
        command: cargo run
        working_dir: ./api
        depends_on: [migrate]

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
- [ ] Resolve `working_dir` and template paths against the yaml file's directory
- [ ] HTTP/TCP health checks with readiness gating
- [ ] Template rendering for `.arig/templates` -> `.arig/generated`
- [ ] Dynamic env injection from dependency metadata

Command surface:
- [ ] Implement `arig down`
- [ ] `--format json` structured output
- [ ] Single-service commands (`status`, `logs`, `env`, `restart`, `build`)

Plugin platform:
- [ ] Docker runtime via bollard
- [ ] Helm/k8s publish plugins
- [ ] External plugin protocol
- [ ] `arig mcp` server

## License

Dual-licensed under either of MIT or Apache-2.0, at your option.
