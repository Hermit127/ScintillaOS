#!/bin/sh
set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
image=${FINAL_2026_DOCKER_IMAGE:-zhouzhouyi/os-contest:20260510}

test_image=
expect_test_image=0
for argument in "$@"; do
    if [ "$expect_test_image" -eq 1 ]; then
        test_image=$argument
        break
    fi
    if [ "$argument" = "--test-image" ]; then
        expect_test_image=1
    fi
done

if [ -n "$test_image" ]; then
    test_dir=$(CDPATH= cd -- "$(dirname -- "$test_image")" && pwd)
    exec docker run --rm \
        --user "$(id -u):$(id -g)" \
        -e HOME=/tmp \
        -v "$repo:/work" \
        -v "$test_dir:$test_dir:ro" \
        -w /work \
        --entrypoint python3 \
        "$image" \
        scripts/validate_loongarch12_boot.py "$@"
fi

exec docker run --rm \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -v "$repo:/work" \
    -w /work \
    --entrypoint python3 \
    "$image" \
    scripts/validate_loongarch12_boot.py "$@"
