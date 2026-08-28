#!/usr/bin/env node
/** Stable operator entry point for the read-only PostgreSQL benchmark. */

import { main } from "../tests/load/postgres_explain.ts";

process.exitCode = main(process.argv.slice(2));
