#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
#
# Runs sccion's tests on Linux from macOS (or anywhere with a container
# runtime), the Docker dist tests included. See README.md beside it.
#
# Usage: scripts/linux-tests/run.sh ["<cargo test args>" ...]
set -o errexit -o nounset -o pipefail

here=$(cd "$(dirname "$0")" && pwd)
src=$(cd "$here/../.." && pwd)
image=sccion-linux-tests
logs=$src/target/linux-tests

if [ $# = 0 ]; then
  # What CI's dist-tests and dist-server test jobs run.
  set -- \
    "--no-default-features --features=dist-tests test_dist_ -- --test-threads 1" \
    "--no-fail-fast --lib --bins --tests --features=dist-server"
fi

# Apple's `container` when its service runs, Docker otherwise;
# SCCION_LINUX_TESTS_RUNTIME=docker|container chooses.
runtime=${SCCION_LINUX_TESTS_RUNTIME:-}
if [ -z "$runtime" ]; then
  if command -v container >/dev/null && container system status >/dev/null 2>&1; then
    runtime=container
  else
    runtime=docker
  fi
fi

case $runtime in
  container)
    # One VM per container: give it room to build sccion, and the
    # capabilities dockerd needs. Named volumes must exist beforehand.
    run=(container run --cap-add ALL -m 8G -c 8)
    for v in target cargo docker tmp; do
      container volume inspect "$image-$v" >/dev/null 2>&1 \
        || container volume create "$image-$v" >/dev/null
    done
    ;;
  docker)
    run=(docker run --privileged)
    ;;
  *)
    echo "unknown runtime '$runtime'" >&2
    exit 2
    ;;
esac

$runtime image inspect "$image" >/dev/null 2>&1 \
  || $runtime build -t "$image" "$here"

mkdir -p "$logs"
# /tmp on a volume rather than the container's root: the dist build server's
# overlay upper directory lives in the tests' temp dir, and the kernel refuses
# an overlay root as an overlay's upper layer -- which Docker Desktop's
# container roots are. The volumes keep the build, the crates, the inner
# Docker's images and the temp dir between runs.
exec "${run[@]}" --rm \
  -v "$src":/src:ro -v "$here":/t:ro -v "$logs":/logs \
  -v "$image-target":/target -v "$image-cargo":/usr/local/cargo/registry \
  -v "$image-docker":/var/lib/docker -v "$image-tmp":/tmp \
  -e CARGO_TARGET_DIR=/target -e CARGO_PROFILE_DEV_DEBUG=0 \
  "$image" /t/in-container.sh "$@"
