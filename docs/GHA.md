# GitHub Actions

To use the [GitHub Actions cache](https://docs.github.com/en/actions/using-workflows/caching-dependencies-to-speed-up-workflows), you need to set `SCCION_GHA_ENABLED` to `on` to enable it.

By changing `SCCION_GHA_VERSION`, we can purge all the cache.

This cache type needs `ACTIONS_RESULTS_URL` and `ACTIONS_RUNTIME_TOKEN`, which GitHub gives to JavaScript actions but not to `run:` steps, and `ACTIONS_CACHE_SERVICE_V2`, without which sccion speaks the retired v1 cache API and every write fails while the build still passes. The following step sets all three for the rest of the job.

```yaml
- name: Configure sccion
  uses: actions/github-script@v7
  with:
    script: |
      core.exportVariable('ACTIONS_RESULTS_URL', process.env.ACTIONS_RESULTS_URL || '');
      core.exportVariable('ACTIONS_RUNTIME_TOKEN', process.env.ACTIONS_RUNTIME_TOKEN || '');
      core.exportVariable('ACTIONS_CACHE_SERVICE_V2', 'on');
```

The `SCCION_GHA_RW_MODE` environment variable can be set to `READ_ONLY` to make sccion use this backend in read-only mode. The default is `READ_WRITE`.

## Behavior

In case sccion reaches the rate limit of the service, the build will continue, but the storage might not be performed.
