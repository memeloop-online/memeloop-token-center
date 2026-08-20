#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
vendor_root="${repository_root}/vendor/rust_decimal"

fail() {
  echo "rust_decimal vendor contract: $*" >&2
  exit 1
}

expect_equal() {
  local label="$1" actual="$2" expected="$3"
  test "${actual}" = "${expected}" \
    || fail "${label} mismatch: expected ${expected}, got ${actual}"
}

expect_equal "fork manifest SHA-256" \
  "$(sha256sum "${vendor_root}/Cargo.toml" | awk '{print $1}')" \
  "153bad3625511c60b3f6d2fccf5da952063a11efe950e781c287e3fb52324387"
expect_equal "manifest patch SHA-256" \
  "$(sha256sum "${vendor_root}/MEMELOOP-MANIFEST.patch" | awk '{print $1}')" \
  "0c2826a356f71e39f57c79695de8d332dba8d339ce9b5ad8a5a8ea45c8ca2549"

# Reverse the exact checked-in patch and authenticate the resulting normalized
# upstream manifest. This proves the fork changed only the reviewed manifest
# hunks; it is stronger than comparing the fork to a self-authored checksum.
reconstructed_upstream_manifest="$({
  patch --reverse --silent --output=- \
    "${vendor_root}/Cargo.toml" < "${vendor_root}/MEMELOOP-MANIFEST.patch"
} | sha256sum | awk '{print $1}')" \
  || fail "manifest patch could not be reversed"
expect_equal "reconstructed upstream Cargo.toml SHA-256" \
  "${reconstructed_upstream_manifest}" \
  "33cbd9b506cfaa14d1df68bab1af011ceccbd66000a6d68a5ef56ecb776ac9ac"

upstream_tree_digest="$({
  find "${vendor_root}" -type f \
    ! -path "${vendor_root}/Cargo.toml" \
    ! -path "${vendor_root}/Cargo.lock" \
    ! -path "${vendor_root}/MEMELOOP-FORK.md" \
    ! -path "${vendor_root}/MEMELOOP-MANIFEST.patch" -print0 \
    | LC_ALL=C sort -z \
    | xargs -0 sha256sum
} | sed "s#${repository_root}/##" | sha256sum | awk '{print $1}')"
expect_equal "unmodified upstream release tree SHA-256" \
  "${upstream_tree_digest}" \
  "bcc9fba4b64831a9db0aab840e7f0fff25aa70e5529a238f0ff8709cf1b34f4f"

source_tree_digest="$(find "${vendor_root}/src" -type f -print0 \
  | LC_ALL=C sort -z \
  | xargs -0 sha256sum \
  | sed "s#${repository_root}/##" \
  | sha256sum | awk '{print $1}')"
expect_equal "runtime source tree SHA-256" \
  "${source_tree_digest}" \
  "bed83f744adbfb12b004dfd3d4157286bae3e5762bed8d872df99f44e3bcd2f7"

grep -Fq '"sha1": "c7efe1690bd8e460731ff97a7c4941ecffc8751b"' \
  "${vendor_root}/.cargo_vcs_info.json" \
  || fail "upstream crates.io VCS provenance is missing or changed"
grep -Fq 'rkyv = { default-features = false' "${vendor_root}/Cargo.toml.orig" \
  || fail "upstream source manifest no longer records the removed rkyv dependency"
! grep -Eq '^(rkyv|rkyv-safe) =|^\[dependencies\.rkyv\]|dev-dependencies\.rkyv' \
  "${vendor_root}/Cargo.toml" \
  || fail "rkyv remains reachable from the fork manifest"
! grep -Fq 'name = "rkyv"' "${repository_root}/Cargo.lock" \
  || fail "rkyv remains in the product lockfile"

# The nested lock is ignored by upstream and must stay absent from a clean
# checkout so generic recursive scanners cannot mistake it for a product lock.
if git -C "${repository_root}" ls-files --error-unmatch \
  vendor/rust_decimal/Cargo.lock >/dev/null 2>&1; then
  fail "inactive nested Cargo.lock must not be committed"
fi

grep -Fq 'be2a24f50780bc85f09cc6ac299bdf1424302742d77221106859c9d8b102126a' \
  "${vendor_root}/MEMELOOP-FORK.md" \
  || fail "published crates.io archive checksum is missing"

echo "rust_decimal vendor contract: passed"
