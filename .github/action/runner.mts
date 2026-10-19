// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// What the action's two steps share: its inputs, and the runner's workflow
// commands -- which are no more than lines appended to files the runner names,
// so there is nothing here an @actions/core would add but a dependency.
//
// TypeScript, run as it is: the runner starts the file named in action.yml
// with its own Node 24, which strips the types itself. Nothing is compiled and
// nothing is bundled, so these files are exactly what runs. That allows only
// syntax Node can erase -- no `enum`, no `namespace`, no parameter properties --
// which `tsc` with `erasableSyntaxOnly` checks (see package.json).

import * as fs from 'node:fs';

/**
 * An input as the runner passes it: `INPUT_` and the name uppercased, hyphens
 * kept, so `gha-version` arrives as `INPUT_GHA-VERSION`.
 */
export function input(name: string): string {
  return (process.env[`INPUT_${name.toUpperCase()}`] ?? '').trim();
}

export function flag(name: string): boolean {
  return input(name).toLowerCase() === 'true';
}

export type Severity = 'notice' | 'warning' | 'error';

export function annotate(kind: Severity, message: string): void {
  const text = message.replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
  process.stdout.write(`::${kind} title=sccion::${text}\n`);
}

function fileCommand(variable: string, line: string): void {
  const file = process.env[variable];
  if (!file) throw new Error(`${variable} is not set: this runs only as a step on an Actions runner`);
  fs.appendFileSync(file, `${line}\n`);
}

function assignment(variable: string, name: string, value: string): void {
  // One line per value is what makes a plain `name=value` safe; a value that
  // could span lines could also smuggle in a second assignment.
  if (/[\r\n]/.test(value)) throw new Error(`${name} would span lines`);
  fileCommand(variable, `${name}=${value}`);
}

export const setEnv = (name: string, value: string): void => assignment('GITHUB_ENV', name, value);
export const setOutput = (name: string, value: string): void => assignment('GITHUB_OUTPUT', name, value);
export const saveState = (name: string, value: string): void => assignment('GITHUB_STATE', name, value);
export const addPath = (dir: string): void => fileCommand('GITHUB_PATH', dir);

export function appendSummary(markdown: string): void {
  const file = process.env.GITHUB_STEP_SUMMARY;
  if (file) fs.appendFileSync(file, markdown);
}
