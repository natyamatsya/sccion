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

## Compose integration tests

`compose.py` runs the tests in `tests/integration` -- what
`make -C tests/integration test-<name>` runs with Docker Compose -- under
Apple's `container`:

```sh
scripts/linux-tests/compose.py basedirs            # make test-basedirs
scripts/linux-tests/compose.py redis s3 cmake      # several, one after another
```

It needs `container` with its service running, `container-compose` and `yq`
(`brew install container-compose yq`). It builds a Linux sccion into
`target/linux-integration` (with `--all-features`, as the Makefile does), then
for each test:

1. starts the services the Makefile lists for it (`SERVICES_<name>`) with
   container-compose;
2. waits for the ones the test depends on: healthy, or finished;
3. runs the test service itself with `container run`, and takes its exit
   code;
4. stops and removes the services again.

What it works around in container-compose 1.2.0, rewriting a copy of the
compose file rather than the file itself:

- **No variable substitution.** `${SCCACHE_PATH:-...}` reaches the container as
  it stands. The copy has every `${...}` expanded the way Compose does.
- **String commands are one argument.** `command: --port :9000 --access ...`
  becomes a single flag. The copy splits them the way Compose does.
- **`tmpfs:` is ignored.** A service's tmpfs directories are made before it
  starts instead; the test service gets real ones from `container run --tmpfs`.
- **`service_completed_successfully` never holds,** because `container` keeps
  no exit code for a stopped container. The runner waits for such a service to
  stop instead.
- **No service names in DNS** without `sudo container system dns create
  integration`. The test container gets the services' addresses in
  `/etc/hosts`, as container-compose does for its own containers.

The test service runs as root rather than as your user: the `/etc/hosts`
entries need root, and a fresh tmpfs belongs to root. `WITH_COVERAGE` is not
supported.

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
