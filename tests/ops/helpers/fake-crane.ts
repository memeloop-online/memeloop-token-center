#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const fixtures = process.env.FAKE_CRANE_FIXTURES;
if (fixtures === undefined) process.exit(9);
const [command, reference] = process.argv.slice(2);
const mapping = JSON.parse(readFileSync(join(fixtures, 'mapping.json'), 'utf8')) as Record<string, string>;
const key = `${command ?? ''}|${reference ?? ''}`;
const output = mapping[key];
if (output === undefined) process.exit(9);
process.stdout.write(output.startsWith('file:') ? readFileSync(join(fixtures, output.slice(5))) : `${output}\n`);
