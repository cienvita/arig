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

## License

Dual-licensed under either of MIT or Apache-2.0, at your option.
