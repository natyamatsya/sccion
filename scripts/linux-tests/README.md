# Linux tests from macOS

`run.sh` runs sccion's tests on Linux in a container, including the Docker
dist tests (`test_dist_*`), which need Linux, a Docker daemon and the build
server's overlay and bubblewrap sandbox. It is what CI's `dist-tests` and
`dist-server` test jobs do, for when CI is not there to do it.

```sh
scripts/linux-tests/run.sh                    # both of those jobs
scripts/linux-tests/run.sh "--test dist"      # any cargo test arguments, one run each
```

Each argument is one `cargo test --locked` run. A summary is printed; the full
output of each run is in `target/linux-tests/`.

## Runtimes

- **Apple's `container`** is used when its service is running
  (`container system start`).
- **Docker** (Docker Desktop on macOS) otherwise.
- `SCCION_LINUX_TESTS_RUNTIME=docker` or `=container` chooses.

The first run builds the image (`sccion-linux-tests`, from the Dockerfile here)
and creates volumes for the build, the crate registry, the inner Docker's images
and the temp dir. Later runs reuse them. To start fresh, delete the image and the
`sccion-linux-tests-*` volumes.

## How it works

An outer privileged container runs a Docker daemon of its own, and the tests
inside it. The dist test harness reaches its scheduler and build server
containers by their IP addresses, which a macOS host cannot do, but a Linux
container whose daemon owns those containers can.

Three things it works around:

- **Overlay on overlay.** The build server mounts an overlay whose upper
  directory is in the tests' temp dir, and the kernel refuses an overlay as an
  overlay's upper layer. A container's own root is one, so `/tmp` is a volume.
- **Apple's `container` VM kernel** has no nf_tables, and `/proc/sys` starts out
  read-only. `in-container.sh` switches dockerd to legacy iptables and remounts
  `/proc/sys` when it finds either.
- **A read-only source.** The source is mounted read-only and copied inside,
  because some tests create temp dirs in the current directory.

The tests run as root here. A test that relies on a file being unwritable will
not see that as root.
