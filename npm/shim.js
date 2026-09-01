#!/usr/bin/env node
'use strict';

// Entry point for the `agit` command. Staying this thin is deliberate: the real logic lives in
// lib/run.js, and every extra line here is one more thing to keep in sync in two places.
require('./lib/run').run();
