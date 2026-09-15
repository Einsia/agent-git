# Connect an executor through a cloud relay

The cloud service must provide the peer relay API. This implementation targets a
single combined dev backend; it does not enable relay routing in staging or
across backend replicas. Desktop and the executor need compatible CLI builds.

On the execution machine, sign in, enroll its daemon identity, and start the
owner daemon:

```sh
agit login --hub https://dev.agent-git.com
agit rc cloud enroll --hub https://dev.agent-git.com --name "My executor"
agit rc local start --detach
```

The executor opens outbound presence and data connections. It needs no public RC
listener or inbound SSH route for cloud sessions. Its existing local owner
endpoint and session registry remain authoritative. Use the same `AGIT_HOME` for
enrollment, daemon startup, and policy commands if choosing an isolated namespace.

On the Desktop machine, sign in to the same cloud with `agit login --hub ...`.
In Desktop, add a cloud machine, enter the cloud origin, enable cloud connections
for the local controller, and select the online executor. Desktop connects
through its local controller and the relay. Private keys and account/device
credentials stay in the daemon namespace; Desktop stores public device identity.

Enrollment explicitly grants the enrolling owner machine Admin access unless a
machine rule already exists. Cloud connection permission and executor resource
permission are independent: admission alone does not expose another account's
sessions. The executor owner manages its local resource policy:

```sh
agit rc cloud policy
agit rc cloud grant --hub https://dev.agent-git.com \
  --account <immutable-account-id> --resource session:<session-id> --access read
```

Resource selectors are `machine`, `project:<id>`, and `session:<id>`. Access is
`deny`, `read`, `control`, or `admin`. An explicit native or logical session deny
wins over inherited project or machine access. Owner SSH keeps host-owner
semantics and is outside this cloud delegation policy.

`agit rc cloud status --hub ...` shows the public enrollment;
`agit rc cloud devices --hub ...` lists devices visible to the current account.
Detached startup prints the daemon log path. Structured `diagnostics-*.jsonl`
files under the `desktop-rc` directory retain route generations, worker PIDs,
request correlation, policy revisions, and cloud admission stages without RPC
bodies or credentials. Preserve those files from both endpoints when diagnosing
a failure. After an uncertain mutation outcome, inspect the session and its
receipt before sending a new mutation.

Cloud Web RC can later host the shared controller as a separate endpoint. That
host is not part of the Desktop-to-executor route described here.

## Deployed acceptance runner

`tests/desktop/cloud_live.py` creates isolated local and remote daemon namespaces
and reads a development login object from stdin. It exercises a real harness
through the deployed relay, replaces a tunnel worker, checks a second turn's
conversation context, replays events and receipts, and applies an executor
session deny. SSH only prepares and inspects the remote fixture; session RPCs
use the cloud route. The optional `--desktop-test` executable sends another turn
through native Desktop IPC to that same remote harness and verifies its reply.
The runner stops its daemons, revokes its
test devices, signs out its disposable logins, and retains private evidence.
