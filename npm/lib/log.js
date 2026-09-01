'use strict';

// All diagnostics go to stderr: the shim forwards the real binary's stdout, and
// the wrapper must not mix one word of its own into it. The prefix makes it
// obvious at a glance which line the wrapper said and which agit itself said.
const TAG = '[agit]';

module.exports = {
  info: (m) => process.stderr.write(`${TAG} ${m}\n`),
  warn: (m) => process.stderr.write(`${TAG} warning: ${m}\n`),
  error: (m) => process.stderr.write(`${TAG} error: ${m}\n`),
};
