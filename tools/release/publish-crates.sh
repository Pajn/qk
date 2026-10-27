#!/usr/bin/env bash
# Publishes the workspace's crates to crates.io, leaving out each version the
# registry already holds, so that a release whose publishing stopped partway
# can be run again.
#
#   tools/release/publish-crates.sh --dry-run   # verify what would publish
#   tools/release/publish-crates.sh             # publish, with retries
#
# cargo publishes the crates in dependency order and waits for each to reach
# the registry's index before publishing what depends on it. That wait can
# time out while crates.io is slow, which fails the run with some crates
# published and some not. Each attempt therefore publishes only what is still
# missing, and a failed one is retried after a pause. Publishing needs
# CARGO_REGISTRY_TOKEN; a dry run does not.
set -euo pipefail

attempts=${PUBLISH_ATTEMPTS:-5}
pause=${PUBLISH_PAUSE:-60}

# Each publishable crate of the workspace, as `name version`.
crates() {
    cargo metadata --format-version 1 --no-deps --locked |
        jq -r '.packages[] | select(.publish != []) | "\(.name) \(.version)"'
}

# Whether crates.io holds `name` at `version`. curl retries what it takes as
# transient, such as 429 and 503, and gives each try a time limit. Any answer
# but 200 or 404 is then an error, rather than a reason to publish the crate
# again.
published() {
    status=$(curl --silent --output /dev/null --write-out '%{http_code}' \
        --max-time 30 --retry 5 --retry-max-time 300 \
        --user-agent 'qk release (https://github.com/Pajn/qk)' \
        "https://crates.io/api/v1/crates/$1/$2")
    case $status in
    200) return 0 ;;
    404) return 1 ;;
    *)
        echo "crates.io answered $status for $1 $2" >&2
        exit 1
        ;;
    esac
}

# `-p <name>` for each crate whose version crates.io does not hold yet.
missing() {
    crates | while read -r name version; do
        if published "$name" "$version"; then
            echo "already published: $name $version" >&2
        else
            printf -- '-p %s ' "$name"
        fi
    done
}

if [ "${1:-}" = --dry-run ]; then
    packages=$(missing)
    if [ -z "$packages" ]; then
        echo "every crate is already published"
        exit 0
    fi
    # shellcheck disable=SC2086
    exec cargo publish --locked --dry-run $packages
fi

attempt=1
while :; do
    packages=$(missing)
    if [ -z "$packages" ]; then
        echo "every crate is published"
        exit 0
    fi
    # shellcheck disable=SC2086
    if cargo publish --locked $packages; then
        exit 0
    fi
    if [ "$attempt" -ge "$attempts" ]; then
        echo "publishing failed $attempts times; run the release's crates job again" >&2
        exit 1
    fi
    echo "publishing failed; trying again in $((pause * attempt))s" >&2
    sleep $((pause * attempt))
    attempt=$((attempt + 1))
done
