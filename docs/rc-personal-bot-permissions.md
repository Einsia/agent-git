# Personal bot launch permissions

The `personal-rc-start-mode-v1` capability lets a person's own Feishu bot start a
conversation using that account's saved permission mode, including Bypass. It
applies only to a verified direct-message task on the person's own device.
The controller remains an operator under its existing control ceiling.

The Hub freezes the runtime, project, start identity and effective mode before
dispatch. A retry retains that choice. A device without this capability receives
the existing request with no mode or authorization field; upgrading during a
retry does not change that request. Plan is never carried into another session.

The `personal_rc_authorization` request parameter is a public record ID, not a
credential. The device verifies it through `/api/peer/personal-rc/verify-start`
using its device credential and the current connection grant. The Hub checks the
active bot token, its bound sender, source login, device ownership and project
binding. Copying the ID onto an ordinary login does not authorize a launch.

Only the verified response enters the device's nonwire execution guard. It
matches the exact project, canonical project path, runtime, permission mode, caller
and transformed start ID. The daemon checks the path against its current project
binding when preparing and admitting a queued launch. Normal local policy and
confinement still apply. It grants no exception for
resume, permission changes, turns, approvals, files, terminals or other sessions.
Existing operator access to a session already running in Bypass is unchanged.

Remote revocation is bounded, not instantaneous. Each start request verifies
again, and each proof expires within the current connection lease and the Hub's
short proof lifetime. Admission and queued launch execution both check it.
A refusal observed for the same authenticated controller and record invalidates
its queued proofs across connections; copying the ID from another controller
cannot cancel that work. Parent-connection expiry or observed revocation also
denies queued work. A process already launched is not killed by proof expiry.

The Hub keeps an unresolved start until an exact executor result confirms the
session. Recording that first confirmed Bypass result and its audit entry is one
transaction; replaying the same result creates no second transition. The audit
contains identifiers, mode, time and outcome, never prompts, titles or credentials.


## Attachment scope

Personal bot turns and supplemental inputs retain scoped input attachments. Result attachment export is not supported: the daemon does not prepare output directories, collect generated files, or advertise a result attachment capability. Session commands use their ordinary authorization requirements.
