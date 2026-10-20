# Security

sccion is an experimental personal fork, not a supported product. See the
[fork status](README.md#fork-status-experimental--unsupported). There are no
releases and no security support, and there is no commitment to fix or disclose
anything on any schedule.

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's private vulnerability
reporting: **Report a vulnerability** on this repository's **Security** tab. Do
not open a public pull request about them. Every report will be read, but a fix
is not promised.

If the problem also reproduces in Mozilla's
[sccache](https://github.com/mozilla/sccache), which most of this code comes
from, please report it to Mozilla as well:
<https://www.mozilla.org/en-US/security/>. There it will reach the people who
can fix it for everyone.

## Where to be careful

- **`sccion-dist`** (the distributed-compilation scheduler and build servers)
  accepts network connections, authenticates clients and servers with bearer
  tokens, and runs compilations it is sent inside a bubblewrap sandbox. Treat it
  as experimental too. Run it only on networks you trust, behind your own access
  control, and never with `type = "DANGEROUSLY_INSECURE"` authentication outside
  a test setup.
- **Storage backends** use the credentials you configure for them. Give sccion
  credentials scoped to its cache bucket or namespace, and to nothing else.
- **Anyone who can write to a shared cache can poison it** for everyone who
  reads from it. Share a writable cache only with builds you trust as much as
  your own.
