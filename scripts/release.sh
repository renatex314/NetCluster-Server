#!/usr/bin/env bash
# scripts/release.sh 0.2.1
#
# Bump the version, prove it builds, then tag.
#
# This exists because a hand-written `sed` on Cargo.toml is not enough and once
# was not: the workspace version and the inter-crate dependency requirement had to
# agree, nothing checked, and the tag was pushed before anything built. Everything
# here runs *before* the tag is created, so a broken release cannot leave a tag
# behind pointing at a commit that does not compile.
set -euo pipefail

V=${1:-}
if [[ -z "$V" ]]; then
  echo "usage: scripts/release.sh <version>   e.g. scripts/release.sh 0.2.1" >&2
  exit 1
fi
if [[ ! "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like 1.2.3, got '$V'" >&2
  exit 1
fi

cd "$(dirname "$0")/.."

if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is dirty; commit or stash first" >&2
  git status --short >&2
  exit 1
fi
if git rev-parse "v$V" >/dev/null 2>&1; then
  echo "tag v$V already exists" >&2
  exit 1
fi

echo "==> setting workspace version to $V"
perl -0pi -e "s/^version = \"[0-9]+\.[0-9]+\.[0-9]+\"/version = \"$V\"/m" Cargo.toml
grep -m1 '^version = ' Cargo.toml

# Building refreshes Cargo.lock, which must be committed with the bump: CI builds
# --locked and will reject a lockfile that disagrees with the manifests.
echo "==> building"
cargo build --release --bin netcluster-server
echo "==> checking the lockfile is current"
cargo build --release --locked --bin netcluster-server
echo "==> fmt and clippy"
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
echo "==> tests"
cargo test --release
echo "==> node client"
(cd clients/node && npm test)
echo "==> image builds and serves"
docker build -q -t "netcluster-server:release-$V" . >/dev/null
docker rm -f "release-check-$V" >/dev/null 2>&1 || true
docker run -d --name "release-check-$V" -p 8099:8080 "netcluster-server:release-$V" >/dev/null
for _ in $(seq 1 40); do curl -sf -o /dev/null http://127.0.0.1:8099/healthz && break; sleep 1; done
curl -sf http://127.0.0.1:8099/healthz >/dev/null
docker rm -f "release-check-$V" >/dev/null
docker rmi -f "netcluster-server:release-$V" >/dev/null 2>&1 || true

echo "==> committing and tagging"
git add -A
git commit -q -m "$V"
git tag -a "v$V" -m "v$V"

cat <<MSG

  v$V is tagged locally and everything passed.

  Push it:   git push origin master v$V

  That triggers the release workflow, which builds linux/amd64 and linux/arm64,
  verifies both are in the pushed manifest, runs the published image, and syncs
  the Docker Hub overview.
MSG
