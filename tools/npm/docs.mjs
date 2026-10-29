#!/usr/bin/env node
// Copy the documentation's Markdown sources into the generated @runqk/cli
// package, so an installation carries the documentation of its own version.
import { cpSync, existsSync, rmSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: { packages: { type: 'string' } } });
if (!values.packages) throw new Error('usage: docs.mjs --packages <generated npm directory>');
const main = join(resolve(values.packages), '@runqk/cli');
if (!existsSync(main)) throw new Error(`missing generated package: ${main}`);
const sources = join(dirname(fileURLToPath(import.meta.url)), '../../docs/src');
const target = join(main, 'docs');
rmSync(target, { recursive: true, force: true });
cpSync(sources, target, { recursive: true });
console.log(`copied ${sources} to ${target}`);
