# Detached manual worker lifecycle

Schedule contract v6 lets a host retain the context-bound callback for a
detached manual run until TinyFlows confirms that the worker is quiescent.

The host first calls `PrepareManualRun(scope, request_id)`. Preparation is
inert: it creates no claim, queue row, or callback. Repeating the same request
id in the same scope returns the same live opaque handle. The host then
registers and pins the callback actor with the correct agent context and calls
`StartManualRun(handle, request)`. The start payload binds the handle to that
job, callback, storage scope, and delivery policy; a repeated identical start
is idempotent, while a different payload is refused.

`StartManualRun` may return `Starting` while the module is obtaining the claim
and writing the durable queue row. `Running` means that queue admission is
durable. Hosts should wait for `Running` or a terminal state before deciding
the admission outcome: `Running` and `Completed` prove durable admission,
`Refused` denies admission, and `Failed` may follow execution effects or a
persistence error, so it does not promise that the run never started.
`WaitManualRun` returns an immediate, repeatable snapshot; hosts choose their
own polling cadence. Cancelling a Wait request does not cancel the worker. A
lost or ambiguous Start reply can be recovered by repeating the identical
Start or polling Wait with the known handle. An ordinary caller disconnect
does not cancel accepted work.

For explicit cancellation or host shutdown, call `StopManualRun` and continue
polling Wait until it reports `Terminal` or the typed absent result. Stop is
cooperative: TinyFlows finishes the callback already in flight, stops before
starting further work or retries, and releases or fences the occurrence claim.
`Completed` is published only after terminal persistence commits. `Stopped`
and `Failed` are published only after claim release or fencing is confirmed;
cleanup errors remain `CleanupPending`. A typed absent result means the handle
has expired or been acknowledged and no worker can be started from it. A bus
transport error remains uncertain and does not mean absent.

After observing a terminal result, call `AcknowledgeManualRun`, then release
the callback actor. Prepared handles expire while inert; live workers and
terminal results do not expire automatically. Terminal state remains available
until acknowledgement. Once acknowledged, the old handle never starts again;
preparing the same request id creates a fresh handle.

Handles and terminal snapshots belong to one TinyFlows module generation. A
forced unload or process crash can discard that in-memory state while a durable
claim remains until lease expiry. A crash after an external callback may cause
recovery to repeat the side effect. The lifecycle contract is at-least-once
across ambiguous transport or crash recovery and makes no exactly-once claim.

The existing v5 `RunJobNow` member remains available with its existing wire
behavior. New hosts that need to retain callback ownership or request
cancellation should use the v6 prepare/start/wait/stop/acknowledge lifecycle.
