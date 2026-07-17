'use strict';

// All diagnostics go to stderr so a binary's own stdout is never polluted by
// the wrapper. Prefixed so users can tell wrapper output from tool output.
const TAG = '[agit]';

module.exports = {
  info: (m) => process.stderr.write(`${TAG} ${m}\n`),
  warn: (m) => process.stderr.write(`${TAG} warning: ${m}\n`),
  error: (m) => process.stderr.write(`${TAG} error: ${m}\n`),
};
