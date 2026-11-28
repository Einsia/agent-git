# CLI native Windows dispatch

The native test and package jobs request `windows`, `amd64` and a backend
tag selected before child pipeline creation. Runner 24 is the AWS primary. Runner 21 is
the shared ECS standby; its other tags, pause state and unrelated jobs do not
belong to this queue. Windows cross-lint and tool preparation still run on Linux.

Run `scripts/windows-runner-watch.py /absolute/path/to/state.json` from a
single mini launchd agent at a 30-second interval, with RunAtLoad. Install a
reviewed copy outside the checkout, never code from an incoming pipeline.
The state directory must persist between invocations. Configure `PATH` for
`glab` and `aws`, `GITLAB_HOST`, `GLAB_CONFIG_DIR` for the existing runner
operator, `AWS_PROFILE`, `AWS_REGION`, and `WINDOWS_INSTANCE_ID`. The instance
must carry `Role=agentgit-cli-windows`. No credential belongs in GitLab jobs.

An observed pending job wakes the stopped instance. After EC2 returns to
running, the watcher reopens the runner it drained. The Windows runner service
must start at boot. Fresh GitLab contact after boot admits the routing job
via a job-specific Generic Package receipt. Without a receipt within five
minutes, the router selects the existing ECS tag `vida-windows-desktop`,
even if mini or EC2 is unavailable. Neither runner needs new credentials,
and the watcher never writes to ECS. After ten minutes
without work, pause the primary, then check active jobs again on the next tick
before stopping the instance. A manually paused runner is not woken by design.

Queue and boot time precede the GitLab execution timeout (90 minutes for the
test and 65 minutes for packaging). Standby pickup also depends on ECS capacity.
Use an API or web pipeline on main with `AGIT_CHECK_WINDOWS=1` to exercise both
native jobs on the merged configuration. Record job IDs, runner IDs and UTC
timestamps alongside the watcher's stop/start log; EC2 running is not job pickup.

On mini, inspect `launchctl print gui/$(id -u)/com.agentgit.cli-windows-watch`
and the agent's stdout/stderr. To roll back, unload the launch agent, start the
AWS instance, unpause runner 24 and revert the CI routing change. A route is
fixed once its child is created: if the chosen host is subsequently lost,
retry the routing job and then the downstream trigger after checking that no
native job is still running. Never pause the shared ECS runner.
