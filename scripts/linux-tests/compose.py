#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Runs a compose integration test (`make -C tests/integration test-<name>`)
under Apple's `container`, with container-compose for the services it needs.

Usage: scripts/linux-tests/compose.py <name> [<name> ...]

Each name is one of the Makefile's tests -- `basedirs`, `redis`, `cmake`, ...
Needs `container` (its service running), `container-compose` and `yq`. See
README.md beside this file for what it works around and why.
"""

import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE.parent.parent
INTEGRATION = SRC / "tests" / "integration"
COMPOSE_FILE = INTEGRATION / "docker-compose.yml"
IMAGE = "sccion-linux-tests"
# Beside the Makefile's target/debug, which holds the host's own sccion.
TARGET = "target/linux-integration"
SCCACHE_PATH = f"/sccion/{TARGET}/debug/sccion"
# container-compose names a service's container `<project>-<service>`, the
# project being the compose file's directory.
PROJECT = INTEGRATION.name
NETWORK = "default"
WAIT_SECONDS = 180


def log(message):
    print(f"compose.py: {message}", flush=True)


def run(argv, **kwargs):
    return subprocess.run(argv, check=True, **kwargs)


def output(argv):
    return subprocess.run(argv, check=True, capture_output=True, text=True).stdout


def makefile_list(variable):
    """A `VARIABLE := a b c` of the integration Makefile, as a list."""
    pattern = re.compile(rf"^{re.escape(variable)}\s*:=(.*)$")
    for line in (INTEGRATION / "Makefile").read_text().splitlines():
        if m := pattern.match(line):
            return m.group(1).split()
    return None


def interpolate(value, env):
    """Compose's `${VAR}`, `${VAR:-default}`, `${VAR-default}` and `$$`, which
    container-compose leaves as they stand."""

    def one(m):
        if m.group(0) == "$$":
            return "$"
        name, op, default = m.group(1), m.group(2), m.group(3) or ""
        if op == ":-":
            return env.get(name) or default
        if op == "-":
            return env.get(name, default)
        return env.get(name, "")

    return re.sub(r"\$\$|\$\{(\w+)(?:(:?-)([^}]*))?\}", one, value)


def interpolate_all(node, env):
    if isinstance(node, str):
        return interpolate(node, env)
    if isinstance(node, list):
        return [interpolate_all(v, env) for v in node]
    if isinstance(node, dict):
        return {k: interpolate_all(v, env) for k, v in node.items()}
    return node


def as_argv(value):
    """A command or entrypoint as an argument list. A string is split the way
    Compose splits it; container-compose would pass it as one argument."""
    if value is None:
        return None
    return shlex.split(value) if isinstance(value, str) else list(value)


def host_path(spec):
    """A bind mount with its host side made absolute, so the adapted file can
    live outside the directory its relative paths are relative to."""
    host, sep, rest = spec.partition(":")
    if sep and (host.startswith(".") or host.startswith("/")):
        return str((INTEGRATION / host).resolve()) + sep + rest
    return spec


def image_config(image):
    """The entrypoint and command an image runs when a service names neither."""
    run(["container", "image", "pull", image], stdout=subprocess.DEVNULL)
    info = json.loads(output(["container", "image", "inspect", image]))
    found = {}

    def walk(node):
        if isinstance(node, dict):
            for key, value in node.items():
                if key.lower() in ("entrypoint", "cmd") and key.lower() not in found:
                    found[key.lower()] = value
                walk(value)
        elif isinstance(node, list):
            for value in node:
                walk(value)

    walk(info)
    return found.get("entrypoint") or [], found.get("cmd") or []


def adapt_backing(name, service):
    """A service container-compose starts, rewritten for what it lacks."""
    service = dict(service)
    for key in ("command", "entrypoint"):
        if key in service:
            service[key] = as_argv(service[key])
    if "volumes" in service:
        service["volumes"] = [host_path(v) for v in service["volumes"]]
    # `tmpfs:` is ignored, so the directories would not exist: make them
    # before what the service runs. Compose clears the image's command when a
    # service sets its own entrypoint, and so does this.
    if tmpfs := service.pop("tmpfs", None):
        entrypoint, command = service.get("entrypoint"), service.get("command")
        if entrypoint is None:
            entrypoint, image_command = image_config(service["image"])
            command = command if command is not None else image_command
        paths = " ".join(shlex.quote(t.split(":")[0]) for t in tmpfs)
        service["entrypoint"] = ["/bin/sh", "-c", f'mkdir -p {paths} && exec "$@"', name]
        service["command"] = list(entrypoint) + list(command or [])
    # `service_completed_successfully` is never met: `container` keeps no exit
    # code for a stopped container. The runner waits for those itself.
    for dep in (service.get("depends_on") or {}).values():
        if isinstance(dep, dict) and dep.get("condition") == "service_completed_successfully":
            dep["condition"] = "service_started"
    return service


def container_of(service):
    return f"{PROJECT}-{service}"


def state(container):
    for line in output(["container", "ls", "-a"]).splitlines()[1:]:
        fields = line.split()
        if fields and fields[0] == container:
            return fields[5] if len(fields) > 5 else None, fields[4]
    return None, None


def wait_healthy(service, healthcheck):
    test = healthcheck.get("test")
    if not test or test == ["NONE"]:
        return
    argv = (
        ["sh", "-c", test[1]] if test[0] == "CMD-SHELL" else test[1:] if test[0] == "CMD" else ["sh", "-c", test]
    )
    deadline = time.monotonic() + WAIT_SECONDS
    while time.monotonic() < deadline:
        r = subprocess.run(["container", "exec", container_of(service), *argv], capture_output=True)
        if r.returncode == 0:
            return
        time.sleep(2)
    sys.exit(f"compose.py: {service} did not become healthy within {WAIT_SECONDS} s")


def wait_stopped(service):
    deadline = time.monotonic() + WAIT_SECONDS
    while time.monotonic() < deadline:
        _, st = state(container_of(service))
        if st == "stopped":
            return
        time.sleep(2)
    sys.exit(f"compose.py: {service} did not finish within {WAIT_SECONDS} s")


def build_sccion():
    if subprocess.run(["container", "image", "inspect", IMAGE], capture_output=True).returncode:
        run(["container", "build", "-t", IMAGE, str(HERE)])
    log(f"building a Linux sccion into {TARGET}")
    # --all-features, as the Makefile's `build` target.
    run([
        "container", "run", "--rm", "-m", "8G", "-c", "8",
        "-v", f"{SRC}:/src", "-v", f"{IMAGE}-cargo:/usr/local/cargo/registry",
        "-e", f"CARGO_TARGET_DIR=/src/{TARGET}", "-e", "CARGO_PROFILE_DEV_DEBUG=0",
        "-w", "/src", IMAGE, "cargo", "build", "--locked", "--all-features", "--bin", "sccion",
    ])


def test_image(name, service):
    if "image" in service:
        return service["image"]
    build = service["build"]
    build = {"context": build} if isinstance(build, str) else build
    tag = f"sccion-integration-{name}"
    context = (INTEGRATION / build.get("context", ".")).resolve()
    dockerfile = context / build.get("dockerfile", "Dockerfile")
    run(["container", "build", "-t", tag, "-f", str(dockerfile), str(context)])
    return tag


def run_test(name, service, hosts):
    """The test service under `container run`, which has what container-compose
    lacks here: `--tmpfs`, and the container's exit code."""
    image = test_image(name, service)
    argv = ["container", "run", "--rm", "--name", container_of(f"test-{name}"), "--network", NETWORK]
    for spec in service.get("volumes", []):
        argv += ["-v", host_path(spec)]
    # Root, not the service's `user`: the entries below go into /etc/hosts,
    # and a fresh tmpfs belongs to root.
    for spec in service.get("tmpfs", []):
        argv += ["--tmpfs", spec.split(":")[0]]
    if "working_dir" in service:
        argv += ["-w", service["working_dir"]]
    environment = service.get("environment") or {}
    if isinstance(environment, list):
        environment = dict(e.split("=", 1) for e in environment)
    for key, value in environment.items():
        argv += ["-e", f"{key}={'' if value is None else value}"]
    entrypoint = as_argv(service.get("entrypoint")) or []
    command = as_argv(service.get("command")) or []
    # No DNS for the services' names without `container system dns create`, so
    # they go into /etc/hosts, as container-compose does for its own.
    entries = "\n".join(f"{ip} {svc}" for svc, ip in hosts.items())
    script = f'printf "%s\\n" {shlex.quote(entries)} >> /etc/hosts && exec "$@"'
    argv += ["--entrypoint", "/bin/sh", image, "-c", script, f"test-{name}", *entrypoint, *command]
    log(f"running test-{name}")
    return subprocess.run(argv).returncode


def addresses(services):
    hosts = {}
    for line in output(["container", "ls"]).splitlines()[1:]:
        fields = line.split()
        for svc in services:
            if fields and fields[0] == container_of(svc) and len(fields) > 5:
                hosts[svc] = fields[5].split("/")[0]
    return hosts


def one_test(name, compose):
    test = f"test-{name}"
    services = compose["services"]
    if test not in services:
        sys.exit(f"compose.py: the compose file has no {test}")
    profiles = makefile_list(f"PROFILES_{name}") or [name]
    backing = makefile_list(f"SERVICES_{name}") or []

    project_args = [arg for p in profiles for arg in ("--profile", p)]
    with tempfile.TemporaryDirectory() as tmp:
        adapted = {k: v for k, v in compose.items() if k != "services"}
        adapted["services"] = {s: adapt_backing(s, services[s]) for s in backing}
        compose_path = Path(tmp) / "docker-compose.json"
        compose_path.write_text(json.dumps(adapted, indent=2))
        down = ["container-compose", "-f", str(compose_path), *project_args, "down"]
        try:
            if backing:
                log(f"starting {' '.join(backing)}")
                run(["container-compose", "-f", str(compose_path), *project_args, "up", "-d", *backing],
                    cwd=INTEGRATION, stdout=subprocess.DEVNULL)
            for dep, how in (services[test].get("depends_on") or {}).items():
                condition = how.get("condition") if isinstance(how, dict) else "service_started"
                if condition == "service_healthy":
                    wait_healthy(dep, services[dep].get("healthcheck") or {})
                elif condition == "service_completed_successfully":
                    wait_stopped(dep)
            return run_test(name, services[test], addresses(backing))
        finally:
            if backing:
                subprocess.run(down, cwd=INTEGRATION, capture_output=True)
                for svc in backing:
                    subprocess.run(["container", "rm", "-f", container_of(svc)], capture_output=True)


def main():
    names = sys.argv[1:]
    if not names or names[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0 if names else 2
    for tool in ("container", "container-compose", "yq"):
        if subprocess.run(["which", tool], capture_output=True).returncode:
            sys.exit(f"compose.py: needs {tool} on the PATH")
    if subprocess.run(["container", "system", "status"], capture_output=True).returncode:
        sys.exit("compose.py: the container service is not running: container system start")
    if os.environ.get("WITH_COVERAGE"):
        sys.exit("compose.py: WITH_COVERAGE is not supported")

    env = dict(os.environ, UID=str(os.getuid()), GID=str(os.getgid()), SCCACHE_PATH=SCCACHE_PATH)
    exploded = output(["yq", "-o", "json", "--yaml-fix-merge-anchor-to-spec=true", "explode(.)", str(COMPOSE_FILE)])
    compose = interpolate_all(json.loads(exploded), env)

    build_sccion()
    failed = []
    for name in names:
        rc = one_test(name, compose)
        log(f"test-{name}: {'passed' if rc == 0 else f'FAILED (exit {rc})'}")
        if rc:
            failed.append(name)
    if failed:
        log(f"failed: {' '.join(failed)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
