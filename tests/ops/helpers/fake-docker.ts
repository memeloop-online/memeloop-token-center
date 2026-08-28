#!/usr/bin/env node
import { appendFileSync } from 'node:fs';

const arguments_ = process.argv.slice(2);
if (process.env.FAKE_DOCKER_LOG !== undefined) appendFileSync(process.env.FAKE_DOCKER_LOG, `${JSON.stringify(arguments_)}\n`);
if (arguments_[0] !== 'buildx' || arguments_[1] !== 'imagetools' || arguments_[2] !== 'inspect') process.exit(9);
if (arguments_.includes('--format')) {
  const source = process.env.FAKE_IMAGE_SOURCE;
  const revision = process.env.FAKE_IMAGE_REVISION;
  if (source === undefined || revision === undefined) process.exit(9);
  process.stdout.write(`${JSON.stringify({ config: { Labels: { 'org.opencontainers.image.source': source, 'org.opencontainers.image.revision': revision } } })}\n`);
} else process.stdout.write('immutable image exists\n');
