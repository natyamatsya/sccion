// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// The action's main step: install sccion, start its server holding the Actions
// cache credentials, and point the rest of the job at it.
//
// The credentials are why this is a JavaScript action at all. The runner hands
// ACTIONS_RUNTIME_TOKEN and ACTIONS_RESULTS_URL to Node actions only -- not to
// `run:` steps, not to composite actions -- and they go from here to the
// server's environment and nowhere else. Never to GITHUB_ENV, where every later
// step, build script and test binary would inherit a token that also mints the
// job's OIDC tokens.

import * as fs from 'node:fs';
import * as path from 'node:path';
import * as crypto from 'node:crypto';
import { spawnSync } from 'node:child_process';

import { addPath, annotate, flag, input, saveState, setEnv, setOutput } from './runner.mts';

const REPO = input('repository') || 'natyamatsya/sccion';
const API = 'https://api.github.com';
const USER_AGENT = 'natyamatsya-sccion-action';
const WINDOWS = process.platform === 'win32';
const EXE = WINDOWS ? 'sccion.exe' : 'sccion';

function runnerTemp(): string {
  const temp = process.env.RUNNER_TEMP;
  if (!temp) throw new Error('RUNNER_TEMP is not set: this runs only as a step on an Actions runner');
  return temp;
}

/** What ci.yml's `build` job ships, keyed by what Node calls the runner. */
const TARGETS: Readonly<Record<string, string>> = {
  'linux-x64': 'x86_64-unknown-linux-musl',
  'linux-arm64': 'aarch64-unknown-linux-musl',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
  'win32-arm64': 'aarch64-pc-windows-msvc',
};

// --- Finding and fetching a release -----------------------------------------

interface Asset {
  name: string;
  /** The API URL, which answers the bytes when asked for application/octet-stream. */
  url: string;
}

interface Release {
  tag_name: string;
  draft: boolean;
  assets: Asset[];
}

const NOT_VISIBLE =
  'A repository answers 404, not 403, to a token that cannot read it. If ' +
  `${REPO} is private, github.token reaches it only from its own workflows; anywhere ` +
  'else, pass a token with Contents: read on it. A draft release is visible only to a ' +
  'token with push access.';

async function get(url: string, token: string, accept = 'application/vnd.github+json'): Promise<Response> {
  const headers: Record<string, string> = {
    Accept: accept,
    'User-Agent': USER_AGENT,
    'X-GitHub-Api-Version': '2022-11-28',
  };
  if (token) headers.Authorization = `Bearer ${token}`;
  for (let attempt = 1; ; attempt++) {
    try {
      let res = await fetch(url, { headers, redirect: 'manual' });
      // An asset answers with a redirect to signed storage, followed by hand and
      // without the token: the signature is the credential there, and the token
      // has no business leaving api.github.com.
      const location = res.status >= 300 && res.status < 400 ? res.headers.get('location') : null;
      if (location) res = await fetch(location, { headers: { 'User-Agent': USER_AGENT } });
      if (res.status < 500 || attempt === 3) return res;
    } catch (err) {
      if (attempt === 3) throw err;
    }
    await new Promise((resolve) => setTimeout(resolve, 2000 * attempt));
  }
}

async function release(tag: string, token: string): Promise<Release> {
  const at = (suffix: string) => get(`${API}/repos/${REPO}/releases${suffix}`, token);
  if (tag === 'latest') {
    const res = await at('/latest');
    if (res.ok) return (await res.json()) as Release;
    throw new Error(`no published release of ${REPO} is visible (HTTP ${res.status}). ${NOT_VISIBLE}`);
  }
  const res = await at(`/tags/${encodeURIComponent(tag)}`);
  if (res.ok) return (await res.json()) as Release;
  // A draft cannot be looked up by its tag, only found in the list.
  const list = await at('?per_page=100');
  const draft = list.ok ? ((await list.json()) as Release[]).find((r) => r.tag_name === tag) : undefined;
  if (draft) {
    annotate('notice', `${REPO} ${tag} is a draft release; installing it anyway`);
    return draft;
  }
  throw new Error(`${REPO} has no release ${tag} visible to this token (HTTP ${res.status}). ${NOT_VISIBLE}`);
}

async function fetchAsset(rel: Release, name: string, token: string): Promise<Buffer | null> {
  const asset = rel.assets.find((a) => a.name === name);
  if (!asset) return null;
  const res = await get(asset.url, token, 'application/octet-stream');
  if (!res.ok) throw new Error(`downloading ${name}: HTTP ${res.status}`);
  return Buffer.from(await res.arrayBuffer());
}

/** `<hex>  <archive>` lines as sha256sum prints them, or a single bare digest. */
function pinnedDigest(text: string, archive: string): string {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  const [only] = lines;
  if (lines.length === 1 && only !== undefined && /^[0-9a-f]{64}$/i.test(only)) return only.toLowerCase();
  for (const line of lines) {
    const [, digest, file] = /^([0-9a-f]{64})\s+\*?(\S+)$/i.exec(line) ?? [];
    if (digest !== undefined && file === archive) return digest.toLowerCase();
  }
  if (lines.length) throw new Error(`the sha256 input pins no digest for ${archive}`);
  return '';
}

async function install(tag: string, target: string, token: string): Promise<string> {
  const name = `sccion-${tag}-${target}`;
  const archive = `${name}.tar.gz`;
  const pinned = pinnedDigest(input('sha256'), archive);

  // The runner's tool cache, laid out as @actions/tool-cache lays it out: empty
  // every time on a hosted runner, a saved download on a self-hosted one. The
  // marker holds the archive's digest, so a pin still means something when the
  // archive is not fetched again.
  const root = process.env.RUNNER_TOOL_CACHE || path.join(runnerTemp(), 'tools');
  const dir = path.join(root, 'sccion', tag.replace(/^v/, ''), process.arch);
  const exe = path.join(dir, EXE);
  const marker = `${dir}.complete`;
  if (fs.existsSync(marker) && fs.existsSync(exe) && (!pinned || fs.readFileSync(marker, 'utf8').trim() === pinned)) {
    console.log(`sccion ${tag} from the tool cache: ${exe}`);
    return exe;
  }

  const rel = await release(tag, token);
  const bytes = await fetchAsset(rel, archive, token);
  if (!bytes) throw new Error(`${REPO} ${tag} has no ${archive}: that release was not built for this platform`);
  const digest = crypto.createHash('sha256').update(bytes).digest('hex');
  const published = await fetchAsset(rel, `${archive}.sha256`, token);
  const expected: Array<[from: string, digest: string]> = [];
  if (pinned) expected.push(['the sha256 input', pinned]);
  // The release job writes the digest alone, no file name after it.
  const [publishedDigest] = published ? published.toString('utf8').trim().split(/\s+/) : [];
  if (publishedDigest) expected.push([`${archive}.sha256`, publishedDigest.toLowerCase()]);
  if (!expected.length) {
    throw new Error(`${archive} has no .sha256 beside it and none is pinned; not running a binary nothing vouches for`);
  }
  for (const [from, want] of expected) {
    if (digest !== want) throw new Error(`${archive} has sha256 ${digest}, but ${from} says ${want}`);
  }
  console.log(`${archive}: sha256 ${digest}, matching ${expected.map(([from]) => from).join(' and ')}`);

  const work = fs.mkdtempSync(path.join(runnerTemp(), 'sccion-'));
  fs.writeFileSync(path.join(work, archive), bytes);
  // System32's bsdtar by full path on Windows: Git's GNU tar is often earlier on
  // PATH, and reads `C:` in a path as a remote host.
  const tar = WINDOWS ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe') : 'tar';
  const x = spawnSync(tar, ['-xzf', archive], { cwd: work, encoding: 'utf8' });
  if (x.status !== 0) throw new Error(`extracting ${archive}: ${x.stderr || x.error}`);
  fs.mkdirSync(dir, { recursive: true });
  fs.copyFileSync(path.join(work, name, EXE), exe);
  fs.chmodSync(exe, 0o755);
  fs.writeFileSync(marker, digest);
  fs.rmSync(work, { recursive: true, force: true });
  return exe;
}

/**
 * The release this copy of the action belongs to: the first `version =` in the
 * Cargo.toml beside it, read the way ci.yml's release job reads it. So
 * `natyamatsya/sccion@v0.1.5` installs sccion v0.1.5 without being told.
 */
function bundledTag(): string {
  const toml = fs.readFileSync(path.join(import.meta.dirname, '..', '..', 'Cargo.toml'), 'utf8');
  const m = /^version\s*=\s*"([^"]+)"/m.exec(toml);
  if (!m) throw new Error('no version in the Cargo.toml beside this action; pass `version`');
  return `v${m[1]}`;
}

// --- The server ---------------------------------------------------------------

/**
 * The same executable under the name `sccache`, for RUSTC_WRAPPER.
 *
 * cc-rs puts a build script's C and C++ through RUSTC_WRAPPER only when the
 * wrapper's file name is one it knows -- sccache, cachepot, buildcache, kache --
 * and `sccion` is not one; without this, every `cc`-built C file compiles
 * uncached and nothing says so. sccion answers to `sccache` as its own name
 * (src/cmdline.rs). In a directory of its own, off PATH, so it shadows no real
 * sccache.
 */
function sccacheAlias(exe: string): string {
  const dir = path.join(runnerTemp(), 'sccion-as-sccache');
  const alias = path.join(dir, WINDOWS ? 'sccache.exe' : 'sccache');
  fs.mkdirSync(dir, { recursive: true });
  fs.rmSync(alias, { force: true });
  try {
    fs.linkSync(exe, alias);
  } catch {
    fs.copyFileSync(exe, alias);
    fs.chmodSync(alias, 0o755);
  }
  return alias;
}

// sccion takes the first configured backend in a fixed order (S3, Redis,
// Memcached, GCS, GHA, Azure, WebDAV, OSS, COS), so any of these beside
// `backend: gha` makes the choice something other than what was asked for.
const OTHER_BACKEND =
  /^(SCCION|SCCACHE)_(BUCKET|REDIS|REDIS_ENDPOINT|REDIS_CLUSTER_ENDPOINTS|MEMCACHED|MEMCACHED_ENDPOINT|GCS_BUCKET|AZURE_CONNECTION_STRING|WEBDAV_ENDPOINT|OSS_BUCKET|COS_BUCKET|MULTILEVEL_CHAIN)$/;
const CACHE_CREDENTIALS = ['ACTIONS_RUNTIME_TOKEN', 'ACTIONS_RESULTS_URL', 'ACTIONS_CACHE_URL', 'ACTIONS_CACHE_SERVICE_V2'];

/**
 * This step's environment -- which carries the job's and the step's own `env:`,
 * so every SCCION_* setting reaches the server -- less what it must not hold.
 */
function serverEnv(backend: string, port: string, log: string): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const key of Object.keys(env)) {
    // INPUT_* carries the download token; the ID-token pair mints OIDC tokens.
    if (key.startsWith('INPUT_') || key.startsWith('ACTIONS_ID_TOKEN_REQUEST_') || key === 'ACTIONS_RUNTIME_URL') {
      delete env[key];
    }
  }
  const userGha =
    Object.keys(env).some((k) => /^(SCCION|SCCACHE)_GHA_(ENABLED|VERSION)$/.test(k)) ||
    /gha/i.test(env.SCCION_MULTILEVEL_CHAIN || env.SCCACHE_MULTILEVEL_CHAIN || '');
  if (backend === 'gha') {
    const other = Object.keys(env).filter((k) => OTHER_BACKEND.test(k));
    if (other.length) {
      annotate(
        'warning',
        `backend: gha, but ${other.join(', ')} also configure${other.length === 1 ? 's' : ''} a backend, ` +
          'and sccion uses only one. Use backend: env to let those settings decide.',
      );
    }
    env.SCCION_GHA_ENABLED = 'on';
    if (input('gha-version')) env.SCCION_GHA_VERSION = input('gha-version');
  } else if (backend !== 'env') {
    throw new Error(`backend: ${backend} is neither gha nor env`);
  }
  if (backend === 'gha' || userGha) {
    if (!env.ACTIONS_RUNTIME_TOKEN || !(env.ACTIONS_RESULTS_URL || env.ACTIONS_CACHE_URL)) {
      throw new Error('the runner gave this action no Actions cache credentials (ACTIONS_RUNTIME_TOKEN, ACTIONS_RESULTS_URL)');
    }
    // The runner announces cache v2 only to JavaScript actions, and opendal
    // speaks the retired v1 without it -- every write fails, the build passes.
    env.ACTIONS_CACHE_SERVICE_V2 ||= 'on';
  } else {
    for (const key of CACHE_CREDENTIALS) delete env[key];
  }
  // Never idle out: a server that exits mid-job is restarted by the next
  // compile from that compile's environment, which holds none of the above.
  env.SCCION_IDLE_TIMEOUT = '0';
  if (port) env.SCCION_SERVER_PORT = port;
  env.SCCION_ERROR_LOG = log;
  if (input('log')) env.SCCION_LOG = input('log');
  return env;
}

function startServer(exe: string, env: NodeJS.ProcessEnv, dir: string): void {
  // Whatever answers on this address -- a self-hosted runner's last job, or this
  // action used twice -- holds the credentials it was started with.
  spawnSync(exe, ['--stop-server'], { env, stdio: 'ignore', timeout: 60_000 });
  const out = path.join(dir, 'start.log');
  const fd = fs.openSync(out, 'w');
  // A file, not a pipe: on Unix the daemon inherits these descriptors, and a
  // pipe it held would keep spawnSync waiting for as long as the daemon lives.
  const r = spawnSync(exe, ['--start-server'], { env, stdio: ['ignore', fd, fd], timeout: 60_000 });
  fs.closeSync(fd);
  const text = fs.readFileSync(out, 'utf8');
  process.stdout.write(text);
  if (r.status !== 0) {
    const tail = text.trim().split('\n').slice(-3).join(' / ');
    throw new Error(`sccion --start-server failed (${r.error ?? `exit ${r.status}`}): ${tail}`);
  }
}

async function main(): Promise<void> {
  const token = input('token');
  const backend = input('backend') || 'gha';
  const port = input('port');

  let exe: string;
  let tag: string;
  if (input('binary')) {
    exe = path.resolve(input('binary'));
    tag = 'local';
  } else {
    const platform = `${process.platform}-${process.arch}`;
    const target = TARGETS[platform];
    if (!target) throw new Error(`sccion is not built for ${platform}; it is for ${Object.keys(TARGETS).join(', ')}`);
    tag = input('version') || bundledTag();
    if (tag === 'latest') tag = (await release('latest', token)).tag_name;
    else if (!tag.startsWith('v')) tag = `v${tag}`;
    exe = await install(tag, target, token);
  }

  const dir = path.join(runnerTemp(), `sccion-server-${port || 'default'}`);
  fs.mkdirSync(dir, { recursive: true });
  const log = path.join(dir, 'server.log');
  startServer(exe, serverEnv(backend, port, log), dir);
  saveState('exe', exe);
  saveState('port', port);
  saveState('log', log);

  const wrapper = sccacheAlias(exe);
  setOutput('installed', 'true');
  setOutput('path', exe);
  setOutput('rustc-wrapper', wrapper);
  setOutput('version', tag);

  if (!flag('export-env')) return;
  addPath(path.dirname(exe));
  setEnv('SCCION_PATH', exe);
  if (port) setEnv('SCCION_SERVER_PORT', port);
  // A cache that falls over costs speed, never the build.
  setEnv('SCCION_IGNORE_SERVER_IO_ERROR', '1');
  if (flag('rustc-wrapper')) {
    if (process.env.CARGO_INCREMENTAL === '1') {
      annotate('warning', 'CARGO_INCREMENTAL=1 is set, and sccion refuses to wrap an incremental build; RUSTC_WRAPPER is left unset');
    } else {
      setEnv('RUSTC_WRAPPER', wrapper);
      if (!process.env.CARGO_INCREMENTAL) setEnv('CARGO_INCREMENTAL', '0');
    }
  }
  if (flag('cmake-launcher')) {
    for (const lang of ['C', 'CXX', 'CUDA']) setEnv(`CMAKE_${lang}_COMPILER_LAUNCHER`, exe);
  }
}

main().catch((err: unknown) => {
  // Without its own full stop: the warning below continues the sentence.
  const reason = (err instanceof Error ? err.message : String(err)).replace(/\.$/, '');
  const message = `sccion is not set up: ${reason}`;
  try {
    setOutput('installed', 'false');
  } catch {
    // Nowhere to say it; the annotation below still does.
  }
  if (flag('required')) {
    annotate('error', message);
    process.exitCode = 1;
  } else {
    annotate('warning', `${message}. The job goes on, compiling uncached.`);
  }
});
