#!/usr/bin/env node
'use strict';

// bin entrypoint for the `agit` command. Keep this trivial: all logic lives in
// lib/run.js so both shims share one code path.
require('./lib/run').run('agit');
