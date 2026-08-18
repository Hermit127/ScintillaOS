#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image=${FINAL_2026_DOCKER_IMAGE:-zhouzhouyi/os-contest:20260510}
image_dir=${FINAL_2026_IMAGE_DIR_HOST:-}

if [ -n "$image_dir" ]; then
    exec docker run --rm \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -v "$repo:/work" \
        -v "$image_dir:$image_dir:ro" \
        -w /work \
        --entrypoint python3 \
        "$image" \
        scripts/run_final_2026.py "$@"
fi

exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -v "$repo:/work" \
    -w /work \
    --entrypoint python3 \
    "$image" \
    scripts/run_final_2026.py "$@"
