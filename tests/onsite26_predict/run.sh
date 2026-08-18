#!/bin/sh

set -u

ROOT=/tmp/starry26-run
PASS=0
FAIL=0

rm -rf "$ROOT"
mkdir -p "$ROOT"

run_case() {
    name=$1
    shift
    echo "========== START $name =========="
    if "$@"; then
        PASS=$((PASS + 1))
        echo "STARRY26_CASE_PASS=$name"
    else
        rc=$?
        FAIL=$((FAIL + 1))
        echo "STARRY26_CASE_FAIL=$name rc=$rc"
    fi
    echo "========== END $name =========="
}

versions() {
    uname -a
    git --version
    vim --version | sed -n '1p'
    gcc --version | sed -n '1p'
    rustc --version
}

git_workflow() {
    repo="$ROOT/repo"
    mkdir "$repo" || return 1
    cd "$repo" || return 1
    git init -q || return 1
    git config user.name StarryOS || return 1
    git config user.email starry26@localhost || return 1
    printf 'base\n' > README.md
    git add README.md && git commit -q -m base || return 1
    git checkout -q -b onsite26 || return 1
    printf 'prediction\n' >> README.md
    git commit -qam prediction || return 1
    git checkout -q master || git checkout -q main || return 1
    git merge -q --no-edit onsite26 || return 1
    git fsck --full || return 1
    test "$(git rev-list --count HEAD)" -eq 2 || return 1
    grep -q prediction README.md
}

c_abi() {
    gcc -O2 -pthread /starry26/kernel_probe.c -o "$ROOT/kernel_probe" || return 1
    "$ROOT/kernel_probe" "$ROOT"
}

rust_runtime() {
    rustc -O /starry26/rust_probe.rs -o "$ROOT/rust_probe" || return 1
    mkdir "$ROOT/rust-output" || return 1
    "$ROOT/rust_probe" "$ROOT/rust-output"
}

fs_concurrency() {
    mkdir "$ROOT/fs" || return 1
    pids=""
    worker=0
    while test "$worker" -lt 8; do
        (
            i=0
            while test "$i" -lt 64; do
                file="$ROOT/fs/$worker-$i.tmp"
                printf '%s:%s\n' "$worker" "$i" > "$file" || exit 1
                mv "$file" "$ROOT/fs/$worker-$i.dat" || exit 1
                i=$((i + 1))
            done
        ) &
        pids="$pids $!"
        worker=$((worker + 1))
    done
    for pid in $pids; do wait "$pid" || return 1; done
    count=$(find "$ROOT/fs" -type f | wc -l)
    test "$count" -eq 512 || return 1
    rm -rf "$ROOT/fs"
    test ! -e "$ROOT/fs"
}

network_remote() {
    if test -z "${STARRY26_REMOTE_URL:-}"; then
        echo "STARRY26_NETWORK_SKIPPED=set STARRY26_REMOTE_URL to enable"
        return 0
    fi
    git ls-remote "$STARRY26_REMOTE_URL" HEAD | grep -q HEAD
}

echo "#### STARRYOS ONSITE 2026 PREDICTION START ####"
run_case versions versions
run_case git_workflow git_workflow
run_case c_abi c_abi
run_case rust_runtime rust_runtime
run_case fs_concurrency fs_concurrency
run_case network_remote network_remote
echo "STARRY26_SUMMARY pass=$PASS fail=$FAIL"

if test "$FAIL" -eq 0; then
    echo "#### STARRYOS ONSITE 2026 PREDICTION PASS ####"
    exit 0
fi
echo "#### STARRYOS ONSITE 2026 PREDICTION FAIL ####"
exit 1
