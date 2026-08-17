#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image=${FINAL_2026_DOCKER_IMAGE:-zhouzhouyi/os-contest:20260510}

exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -v "$repo:/work" \
    -w /work \
    --entrypoint python3 \
    "$image" \
    scripts/validate_loongarch12_boot.py "$@"
