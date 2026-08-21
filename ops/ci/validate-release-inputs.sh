#!/bin/sh
set -eu

fail() {
  echo "release input validation: $*" >&2
  exit 1
}

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
registry=${1:-}
revision=${2:-}
tag_style=${3:-exact}

case "$registry" in
  '' | *[!a-z0-9./_-]* | /* | */ | *//* | *..*)
    fail "registry prefix must be a lowercase OCI host/path without a scheme"
    ;;
esac
case "$revision" in
  *[!0-9a-f]* | '') fail "revision must be a lowercase hexadecimal commit SHA" ;;
esac
[ "${#revision}" -eq 40 ] || fail "revision must contain exactly 40 hexadecimal characters"
case "$tag_style" in
  exact) tag=$revision ;;
  prefixed) tag=sha-$revision ;;
  *) fail "tag style must be exact or prefixed" ;;
esac

cd "$repository"
resolved=$(git rev-parse HEAD)
[ "$resolved" = "$revision" ] || fail "checkout is $resolved, expected $revision"
[ -z "$(git status --porcelain=v1 --untracked-files=all)" ] || \
  fail "release checkout contains tracked or untracked changes"

for path in Dockerfile Dockerfile.importer Dockerfile.plugin-installer; do
  [ -f "$path" ] || fail "$path is missing"
done

cat <<EOF
service|Dockerfile|memeloop-token-center|${registry}/memeloop-token-center:${tag}
importer|Dockerfile.importer|memeloop-token-center-importer|${registry}/memeloop-token-center-importer:${tag}
plugin-installer|Dockerfile.plugin-installer|memeloop-token-center-plugin-installer|${registry}/memeloop-token-center-plugin-installer:${tag}
EOF
