// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// The action's post step, after every other step of the job: say what the
// server did, then stop it. It never fails the job. The build has had its
// verdict, and this is commentary on it -- but commentary that says so, in an
// annotation, when the cache wrote nothing, because a cache that stores nothing
// is otherwise invisible: the build passes, only slower.

import * as fs from 'node:fs';
import { spawnSync } from 'node:child_process';

import { annotate, appendSummary } from './runner.mts';

const exe = process.env.STATE_exe;
const port = process.env.STATE_port;
const log = process.env.STATE_log;

interface Counts {
  counts: Record<string, number>;
}

interface Duration {
  secs: number;
  nanos: number;
}

/** The part of `--show-stats --stats-format=json` read here. */
interface StatsJson {
  cache_location: string;
  stats: {
    cache_hits: Counts;
    cache_misses: Counts;
    cache_errors: Counts;
    cache_writes: number;
    cache_write_errors: number;
    cache_read_errors: number;
    cache_timeouts: number;
    compile_requests: number;
    requests_not_cacheable: number;
    cache_write_duration: Duration | null;
    cache_read_hit_duration: Duration | null;
    not_cached: Record<string, number> | null;
  };
}

// Neither the action's inputs nor any credential: these commands only talk to
// the server, which already holds what it needs.
const env: NodeJS.ProcessEnv = Object.fromEntries(
  Object.entries(process.env).filter(([k]) => !k.startsWith('INPUT_') && !k.startsWith('ACTIONS_')),
);
if (port) env.SCCION_SERVER_PORT = port;

function sccion(...args: string[]) {
  return spawnSync(exe as string, args, { env, encoding: 'utf8', timeout: 120_000 });
}

const count = (c: Counts | undefined): number => Object.values(c?.counts ?? {}).reduce((a, b) => a + b, 0);
const secs = (d: Duration | null | undefined): string => (d ? `${(d.secs + d.nanos / 1e9).toFixed(1)} s` : '-');

function report(): void {
  if (!exe) return; // the main step started nothing
  const json = sccion('--show-stats', '--stats-format=json');
  const stop = sccion('--stop-server');
  if (stop.status !== 0) {
    // --show-stats answers zeros when no server runs; do not report those as fact.
    console.log('No sccion server was running at the end of the job; whatever stopped it reported its counts then.');
    return;
  }
  process.stdout.write(stop.stdout);

  const info = JSON.parse(json.stdout) as StatsJson;
  const s = info.stats;
  const hits = count(s.cache_hits);
  const misses = count(s.cache_misses);
  const errors = count(s.cache_errors);
  const asked = hits + misses + errors;
  const rate = asked ? Math.round((100 * hits) / asked) : 0;
  const job = process.env.GITHUB_JOB ?? 'this job';

  if (process.env.INPUT_SUMMARY !== 'false') {
    annotate(
      'notice',
      `${job}: ${rate}% hits -- ${hits} hits, ${misses} misses, ${errors} errors, ${s.cache_write_errors} failed writes`,
    );
    const rows: Array<[string, string | number]> = [
      ['Hit rate', `${rate}%`],
      ['Hits', hits],
      ['Misses', misses],
      ['Errors', errors],
      ['Writes', s.cache_writes],
      ['Failed writes', s.cache_write_errors],
      ['Read errors', s.cache_read_errors],
      ['Timeouts', s.cache_timeouts],
      ['Not cacheable', s.requests_not_cacheable],
      ['Compile requests', s.compile_requests],
      ['Time writing the cache', secs(s.cache_write_duration)],
      ['Time reading hits', secs(s.cache_read_hit_duration)],
    ];
    const why = Object.entries(s.not_cached ?? {})
      .sort((a, b) => b[1] - a[1])
      .slice(0, 10);
    let md = `### sccion: ${job}\n\n${info.cache_location}\n\n| | |\n|---|--:|\n`;
    md += rows.map(([k, v]) => `| ${k} | ${v} |`).join('\n') + '\n';
    if (why.length) {
      md += '\n**Why not cached**\n\n| reason | compiles |\n|---|--:|\n';
      md += why.map(([k, v]) => `| \`${k}\` | ${v} |`).join('\n') + '\n';
    }
    md += `\n<details><summary>sccion --stop-server</summary>\n\n\`\`\`\n${stop.stdout}\n\`\`\`\n</details>\n\n`;
    appendSummary(md);
  }

  const failedReads = errors + s.cache_read_errors;
  if (s.cache_write_errors || failedReads) {
    const lines =
      log && fs.existsSync(log)
        ? fs
            .readFileSync(log, 'utf8')
            .split('\n')
            .filter((l) => /error|fail/i.test(l))
            .slice(0, 5)
        : [];
    annotate(
      'warning',
      `${job}: ${s.cache_write_errors} cache writes and ${failedReads} reads failed, so later runs will miss.` +
        (lines.length ? ` From the server log:\n${lines.join('\n')}` : ` The server log is ${log}.`),
    );
  }
}

try {
  report();
} catch (err) {
  annotate('warning', `post step: ${err instanceof Error ? err.message : String(err)}`);
}
