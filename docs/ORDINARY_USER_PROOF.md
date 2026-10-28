# Disposable Windows ordinary-user proof

The manual `Ordinary user environment proof` workflow prepares an environment
qualification experiment for the MSI lifecycle gate. It does not install Kwil,
qualify an MSI, or publish a release. Provisioning and dispatch require the
owner's concrete approval; an approval-reference input only correlates that
decision and cannot grant it.

Each selected native GitHub-hosted x64 or ARM64 VM makes one attempt. It creates
one fresh, non-administrator local user with one pointer-backed, memory-only
credential. The complete proof controller and its one worker run as that user.
Independent observations must agree on Medium integrity, absence of admin and
powerful privileges, user/authentication/logon/session identity, the actual
process creation identities and profile continuity. A restricted high-integrity
administrator is not a substitute.

For that fresh SID only, the bootstrap temporarily adds direct noninheriting
allow ACEs to the existing WinSta0 and Default objects: masks `0x22` and `0x83`,
with ACE flags zero. The masks are fixed, not iteratively expanded. Writes are
DACL-only. Original descriptor bytes, ACE order and control state are retained.
Read-only owner/group queries form valid AccessCheck input without granting
owner/group write authority.

Restoration requires known process quiescence and the current DACL to equal
either the captured original or the exact installed delta. A foreign state
causes zero restoration writes. Comparison and restoration are not an atomic
compare-and-swap, so shared-desktop exposure and a restoration race remain
explicit experiment risks.

The supervisor owns the helper before credential creation. The helper adopts
returned controller handles before inspection or receipt IO; transfer requires
the supervisor's containment acknowledgment before resume. Pending broker
activity remains ambiguous even if the job looks empty. Known handles have one
absolute deadline and bounded cumulative cleanup. The worker has at most
20 seconds. Late success, nonzero exit, forced termination, failed close,
missing/foreign receipts and extra job activity cannot qualify the environment.

Reachable credential cleanup zeros before freeing. A forcibly interrupted
logon cannot prove credential cleanup or absence of an unknown child. Such
ambiguity refuses restoration and success rather than guessing. The initial
experiment does not delete accounts or profiles; residue remains on the
disposable hosted VM until provider retirement. Provider disposal is not
recorded as successful fixture cleanup.

The workflow is manual-only, owner-actor/main/first-attempt gated, source-pinned
and limited to 15 minutes. It preserves only bounded nonsecret outcome data.
It has no arbitrary command, account or source-ref input and no retry route.
No operation targets the owner's PC.

ROOT's inert validation compiles the C# and exercises the live-used ownership,
deadline, credential, job, descriptor and receipt paths with fake native calls.
It includes failure controls and compiling mutants. These checks do not execute
P/Invokes, create a user or credential, change ACLs, log on, or dispatch a job.
They do not establish that the fixed desktop rights, profile behavior or job
containment are sufficient on hosted Windows.

Even a successful environment proof leaves the five installer lifecycle gaps
unwaived: rollback after predecessor removal, new upgrade resources, direct
foreign-value collisions, Explorer/icon/GUI execution, and reparse/registry-link,
corrupt-cache and architecture-switch refusals. The lifecycle's unconditional
qualification failure remains until those gates are implemented and pass.
