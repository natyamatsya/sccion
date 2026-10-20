# GitHub Actions

## With the sccion action

This repository is also a GitHub Action. It installs sccion, starts its server
with the Actions cache as the backend, and points the rest of the job at it:

```yaml
- uses: natyamatsya/sccion@v0.17.0
- run: cargo build
```

What it does:

- **Installs** the release the action is tagged with (`@v0.17.0` installs sccion
  v0.17.0), downloaded through the GitHub API and checked against the release's
  published SHA-256 -- and against `sha256:` if you pin digests yourself.
- **Starts the server** with the Actions cache credentials in its environment
  and nowhere else. They are not written to `GITHUB_ENV`, so later steps, build
  scripts and test binaries never see them; the runtime token is the same one
  that requests the job's OIDC tokens.
- **Exports** `RUSTC_WRAPPER`, `SCCION_PATH` and `PATH` for the steps that follow.
  `RUSTC_WRAPPER` names the binary as `sccache`, because cc-rs routes a build
  script's C and C++ through the wrapper only when it has a name it recognises.
  `cmake-launcher: true` also sets `CMAKE_{C,CXX,CUDA}_COMPILER_LAUNCHER`.
- **Reports** at the end of the job: hits, misses and failed writes as an
  annotation and in the job summary, with a warning quoting the server's log
  when the cache could not be written -- the one failure a build never shows.

If sccion cannot be set up -- no release for the runner's platform, a token
that cannot read the release, a fork's pull request without secrets -- the step
warns and the job builds uncached. `required: true` makes that a failure.

Other backends: `backend: env` leaves the choice to the `SCCION_*` settings in
the job's environment (S3, Redis, WebDAV, Azure and the rest; see
[Configuration](Configuration.md)), and passes the Actions credentials to the
server only if those settings ask for the GHA backend.

All inputs and outputs are described in [`action.yml`](../action.yml).

### From another repository

Any repository can use the action. Releases are read through the GitHub API
with the job's own `github.token`, which can read a public repository's
releases, so no secret is needed. Draft releases are the exception: only a
token with push access to `natyamatsya/sccion` sees them.

There are no releases yet -- sccion is an experimental fork that is built from
a checkout (see the [README](../README.md#fork-status-experimental--unsupported)).
Until there are, there is no tag to use either: use the action at `@main` and
pass a sccion you built as `binary:`, and everything else it does works the
same.

## Without the action

To use the [GitHub Actions cache](https://docs.github.com/en/actions/using-workflows/caching-dependencies-to-speed-up-workflows), you need to set `SCCION_GHA_ENABLED` to `on` to enable it.

By changing `SCCION_GHA_VERSION`, we can purge all the cache.

This cache type needs `ACTIONS_RESULTS_URL` and `ACTIONS_RUNTIME_TOKEN`, which GitHub gives to JavaScript actions but not to `run:` steps, and `ACTIONS_CACHE_SERVICE_V2`, without which sccion speaks the retired v1 cache API and every write fails while the build still passes. The following step sets all three for the rest of the job.

```yaml
- name: Configure sccion
  uses: actions/github-script@v9
  with:
    script: |
      core.exportVariable('ACTIONS_RESULTS_URL', process.env.ACTIONS_RESULTS_URL || '');
      core.exportVariable('ACTIONS_RUNTIME_TOKEN', process.env.ACTIONS_RUNTIME_TOKEN || '');
      core.exportVariable('ACTIONS_CACHE_SERVICE_V2', 'on');
```

The `SCCION_GHA_RW_MODE` environment variable can be set to `READ_ONLY` to make sccion use this backend in read-only mode. The default is `READ_WRITE`.

## Behavior

In case sccion reaches the rate limit of the service, the build will continue, but the storage might not be performed.
