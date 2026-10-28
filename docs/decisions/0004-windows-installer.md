# 0004: Windows installer protocol, schema 2

Status: source-only coordinated schema-2 candidate, 2026-10-26. No ICE,
compiler, MSI, CAB, native installation or lifecycle acceptance is claimed.
The machine-readable companion is `scripts/windows-installer-contract.json`.
Both files are normative: a contradiction is a build/acceptance failure, never
permission to choose one interpretation. Changes to this frozen pair require
coordinated backend, wrapper and guard review; incompatible encodings require
a new schema/ABI. The coordinated source migration includes backend, wrapper,
guard and their fixtures; execution and qualification remain separate gates.

The schema-2 source base is `908515ebae3b6e144798d20c7fbdd966bc3d7c51` in
`E:/Development/kwil`, with shared dirty wrap/test/dist work preserved. Reads
included both shared rule files, repository AGENTS/CLAUDE, the full SPEC,
`E:/Development/templates/brief-footer.md`, INSTALL and RELEASING. The brief's
source-only/no-network/no-Git-mutation/private-snapshot ownership overrides the
footer's fetch/commit/push defaults and broad lifecycle bookkeeping. The reviewed
schema-2 contract and explicit file lease govern this source preparation.

## Decision and ownership

`dist.bat` drives a committed-source, offline build of the Windows entry-point
pair and prepares an unsigned native per-user MSI plus a portable ZIP. The
backend authors MSI tables/CAB and inspects them; it never installs. The wrapper
owns the Git snapshot, provenance, builds, static-import gates, immutable input
staging, artifact checksums and final directory publication. The isolated native
guard owns runtime read-only preflight. ROOT owns Cargo/workspace/lock integration,
execution admission and later documentation integration. No new dependencies,
app provenance hook or shared-rule edits are authorized here. `ui-gpu/build.rs`
already accepts `KWIL_BUILD_COMMIT`; the wrapper supplies the archived 40-hex
commit via that existing build mechanism.

This local MSI is a distinct artifact from the existing version-directory
download scripts and raw GitHub Release assets in INSTALL/RELEASING. It changes
none of their names or updater contracts. The repository's current hosted-runner
rule wins over the older self-hosted wording in SPEC. Signing and publication
still need the owner; local checksums are not publisher authentication.

The family/UpgradeCode is permanently
`{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}`. Product and component UUIDv5 namespaces
are distinct fixed values in the companion. New packages use schema 2 and guard ABI 2; component schema stays 1. Both architectures share the destination and family; side-by-side
x64/ARM64 registration is refused. Architecture switching needs explicit
uninstall first. ReleaseVersion is exact ASCII `0.1.0-beta.N`, N=1..65535 without leading
zeros, or exact stable `0.1.0`. Full-string matching rejects whitespace,
build metadata, other prereleases/series and numeric `0.0.N` writer inputs.
Beta derives channel `beta`, MSI version `0.0.N`, and ProductName
`Kwil Beta 0.1.0-beta.N`; stable derives `stable`, `0.1.0`, and
`Kwil 0.1.0`. Compare three numeric MSI components: beta.9 < beta.10,
all betas < stable. No stripping, clamping or wrapping. Future versions require
this contract to be updated. Equal mapped version with a different BuildID
refuses; replacement requires a strictly higher mapped numeric version.
Exact same ProductCode/PackageCode
can enter explicit repair or uninstall. A repack with a fresh PackageCode must
not masquerade as that registered original.

## Backend interface and output

Invoke `pwsh -NoLogo -NoProfile -File scripts/windows-msi.ps1` with exactly six
named arguments, each mandatory and scalar, as listed in the companion. Unknown,
repeated, abbreviated or positional parameters fail. Architecture is lowercase
`x64` or `arm64`, Commit lowercase 40-hex, and ReleaseVersion follows the exact reviewed release grammar
above. The three path arguments are absolute local filesystem paths. Payload
and icon must exist; IconPath must identify the payload's installed `kwil.ico`.
OutputPath and `OutputPath.manifest.json` must both be absent. Symlink/reparse
inputs or output ancestry, same input/output locations, and escaping paths fail.
The wrapper supplies a pre-created exclusive output parent it owns.

Success emits exactly the MSI at OutputPath and its adjacent sidecar, whose name
is the complete MSI filename plus `.manifest.json` (for example
`kwil-x64.msi.manifest.json`). No extra artifact is published. Exit 0 means both
have been closed and read back successfully; exit 1 and an actionable diagnostic
mean failure. Partial outputs are never accepted by the wrapper. Backend staging
and makecab use private owned work, bounded child timeouts and exact PID receipts;
they do not mutate PayloadDirectory. Packaging performs no downloads, installs,
registry/profile inspection or application launch. Timeout cleanup may terminate
only an exact child Process/PID launched and recorded by this invocation
(makecab/expand), without name matching or targeting other process trees.
The runtime guard never terminates applications.

The wrapper must retain the sidecar in the final dist directory, include it in
its outer manifest and SHA256SUMS, and verify every input hash again after the
backend returns. Existing dirty `scripts/dist.ps1` is not yet this protocol: it
does not stage this metadata/guard and its publication allowlist omits the
sidecar. Those are wrapper-owner work, not implicit compatibility or edits here.

## Input: one immutable metadata object

The payload contains installed files at their intended root-I relative paths,
plus `.installer/input.json` and `.installer/kwil-installer-guard.dll`. The latter
two never enter the installed file set or portable ZIP. Portable staging is
separate, includes `kwil.portable` and `kwil.no-update`, and excludes the entire
`.installer` directory. MSI staging includes `kwil.no-update` and never
`kwil.portable`. `kwil.no-update` has the exact UTF-8/no-BOM bytes
`Updates are managed by this distribution. Rebuild/reinstall explicitly.\n`
(the final two printed characters here mean one LF byte); portable marker is an
empty regular file. Input enumeration rejects unknown MSI staging files;
directory walking during packaging is bounded by the companion limits.

The JSON transport is UTF-8, no BOM, at most 1 MiB, depth at most 8. Reject
duplicate member names before object construction, unknown/missing members,
wrong primitive types, invalid UTF-8, NUL, NaN and trailing non-whitespace.
Object member order/whitespace and JSON escaping are irrelevant to identity.
PowerShell ConvertTo-Json output bytes MUST NOT be hashed for BuildID.

Exact top-level members and types:

| Member | Type and meaning |
| --- | --- |
| `schema` | JSON integer, exactly 2 |
| `componentSchema` | JSON integer, exactly 1 |
| `familyGuid` | fixed uppercase braced family GUID |
| `releaseVersion`, `channel`, `msiVersion`, `commit`, `architecture`, `target` | strings; match all arguments and target mapping |
| `guard` | object with exactly `abi` (integer 2), `sha256` (lowercase hex), `imports` (array) |
| `inventory` | complete sorted array of resource objects described below |

Each imports element has exactly `load` and `dll`, both strings. Load is `N` or
`D`; dll is an ASCII lowercase import basename (1..128 bytes), no path separators.
Names must match `[a-z0-9._-]+[.]dll`. The sorted list describes the normal and delay import directories, without
deduplicating a DLL across the two load classes. Zero delay imports is explicit
in that list/count; it cannot mean that delay imports were not inspected.

Each inventory object has exactly the 13 lower-camel-case string members in
`canonicalEncoding.inventoryFields`, including `valueType` as a decimal string.
All are strings even for sizes. Field semantics, sentinels and SQL mapping are
in the companion. There are F (file), R (REG_SZ value) and K (key existence)
records, sorted by normalizedIdentity ordinal ASCII byte order, then id. IDs
are `r000001` onward in that order. SQL column names map positionally to these
fields; there is no JSON parsing inside the guard. The wrapper generates the
complete resource model using the rules below. The backend reconstructs that
model from payload files and fixed schema rules and requires exact equality;
input JSON is evidence, not permission to install arbitrary resources.

Root I means the package directory, P the current-user Programs known folder,
H native64 HKCU. Every ordinary file is root I/source `p`. Root P contains only
`Kwil.lnk`, source `g`, whose bytes/size/hash are generated by the fixed algorithm
below. The generated LNK has no additional payload input file. Backend supplies
its CAB bytes privately. Every F row carries a canonical decimal byte count and
SHA-256, including the LNK and zero-length files. Required root-I files are
`kwil.exe`, `kwil.com`, `kwil.ico`, `LICENSE`, `LICENSES.md`, `LICENSES.txt` and
`kwil.no-update`; schema 1 allows no others. The license filenames match the
current dist payload staging. Missing required files fail.

Installed relative filenames are ASCII, `/` separated, nonempty, without leading
or trailing slash, empty/`.`/`..` segments, drive/UNC/device prefixes, ADS colons,
backslashes, controls, trailing dot/space, or DOS reserved basenames (including
reserved names with extensions). Reject `|`, `[`, `]`, `%`, `$`, `{`, `}`, quotes,
semicolon, `*`, `?`, `<` and `>` in filenames. Key paths use `\` separators and the same segment
safety constraints, except the fixed `*` Classes segment and family GUID braces
are permitted only at their fixed schema locations. Case folding is ONLY ASCII
A-Z to a-z, never Unicode/locale folding. Display spelling remains in `relative`.
Case-insensitive identity collisions and duplicate resource rows fail. Root-I/P
filesystem alias/hardlink collisions fail during backend input inspection.

Registry values are REG_SZ, including exact terminating UTF-16LE NUL in runtime
comparison. Name `@` means the default value; `n:Icon` and `n:Owner` are literal
nonempty names. `~empty~` is the transport representation for a zero-character
REG_SZ; the literal text `~empty~` is not otherwise permitted as registry data.
Nonempty data must not contain NUL or unpaired surrogates. `${I}` is the only
token and expands to the OS-derived canonical root I with exactly one trailing
backslash; no environment expansion is performed for REG_SZ. Thus the shell
command has exactly `"<root>\kwil.exe" "%1"`; the icon value has exactly
`"<root>\kwil.exe",0`. There is no `--` or other argument silently added.

The shell key and its three individual values are fixed in the companion.
For each F resource derive its component UUID first, then add an HKCU key at
`Software\Kwil\Installer\<family>\Components\<component GUID>`, with `Owner`
REG_SZ exactly `<family>|<architecture>|1|<F normalizedIdentity>`. The marker R
and K rows share that F component, rather than creating recursive components.
Each shell R value gets its own R-identity component. A K row exists for every
key directly containing authored values; its component is that of the first
direct R by normalizedIdentity. Shared ancestor keys (Software, Classes, shell,
Kwil/Installer/family/Components) are ancestry checks, not recursively owned
resources. These are the complete registry resources; no separate owner database
or arbitrary per-user registry probe is part of packaging. The complete schema-1
model therefore has exactly 8 F, 11 R and 10 K rows (29 total), 11 components,
8 File rows and 11 Registry rows. Limits larger than these counts bound parsing;
they do not permit extra schema-1 resources.

## Canonical bytes and identity

The companion freezes every field order, prefix and byte encoding. `LP` is a
four-byte unsigned little-endian UTF-8 byte length followed by exactly that many
bytes; empty LP is four zero bytes. Fixed ASCII magic is not length-prefixed.
Numeric metadata fields and resource `valueType` are canonical decimal strings
inside LP; row/import counts and binary lengths are raw u32le. All additions and
conversions use checked arithmetic and the limits are checked before allocation.
No UTF-16 BOM, JSON, locale sorting, newline, timestamp, absolute staging path,
MSI filename, BuildID field, ProductCode or PackageCode enters the KWB2 hash.

Inventory bytes are KWR1 plus count plus all 13 LP fields of every sorted row.
InventorySha256 is their SHA-256. BuildID hashes KWB2 metadata, independently
built guard SHA-256/import evidence and those inventory bytes. ProductCode is
UUIDv5 in the product namespace over KWP2 plus LP architecture/releaseVersion/channel/msiVersion/BuildID.
PackageCode is a fresh UUIDv4 for each packaging call, even a byte-identical
repack. Component UUIDv5 uses KWC1 plus LP architecture/resourceKind/normalized
key identity/componentSchema. Helper R/K marker rows use the F component, as
specified above. Component IDs never include version, commit or file hash, so
unchanged logical ownership survives major upgrades. Changing ownership rules
requires a component-schema change and review of the migration.

UUID namespaces and UUIDv5 input/output use RFC network byte order. In
PowerShell/.NET, manually reorder Guid's mixed-endian representation or parse
hex into network bytes; `Guid.ToByteArray()` is not suitable directly. SHA-1 is
used only for UUIDv5, with version 5 and RFC variant bits set after hashing.
SHA-256 remains the content/BuildID algorithm. Guard binary build inputs contain
family, ABI/schema and static-runtime settings, never BuildID, per-package
inventory, ProductCode or PackageCode. At runtime it reads each active MSI's
tables. This breaks the circular guard-hash/package-identity dependency.

Backend and guard both reconstruct the BuildID and ProductCode from typed MSI
tables, and hash Binary.KwilPreflight to compare its SHA-256. Guard imports in
KwilGuardImport must equal backend PE inspection. ROOT's static artifact checks
also parse both normal and delay imports; field labels alone cannot pass this
gate. The guard need not parse its own loaded image or discover an adjacent JSON.

## Typed MSI protocol and authoring invariants

The companion gives exact custom table column order/types/nullability/primary
keys. Use MSI table creation/import APIs, populate `_Validation`, and use
parameterized MSI records for all data. `s0` does not authorize an unbounded
string. Every custom cell is non-NULL; unused cells contain the literal `-`.
`KwilPackage.Id=1` is the only package row. Count mismatches, extra custom tables
under the Kwil prefix, unknown schemas/kinds/sources or duplicate identities fail.

The standard Property table must contain the companion's immutable constants,
and omit ALLUSERS/SHORTFILENAMES. Actual values replace descriptive placeholders
such as `derived ProductCode`, `Architecture` and `familyGuid`. Guard compares
active database constants with effective properties, including KWIL_* identity,
ProductCode, ProductVersion, UpgradeCode, ProductLanguage, MSIINSTALLPERUSER and
REBOOT. It rejects effective ALLUSERS, SHORTFILENAMES, TRANSFORMS/PATCH or forbidden
feature-selection properties. It also verifies all standard MSI authoring rows
against the resource model: table tampering is not excused by a matching custom
inventory. `Installed`, `UPGRADINGPRODUCTCODE`, KWIL_OLDER/EQUAL/NEWER, UI level and
arbitrary command-line properties never confer ownership or predecessor authority.

The exact Directory rows are TARGETDIR (parent NULL, DefaultDir `SourceDir`),
LocalAppDataFolder (parent TARGETDIR, DefaultDir `.`), INSTALLPROGRAMS (parent
LocalAppDataFolder, DefaultDir `PROGRA~1|Programs`), INSTALLDIR (parent
INSTALLPROGRAMS, DefaultDir `Kwil`), and ProgramMenuFolder (parent TARGETDIR,
DefaultDir `.`). LocalAppDataFolder and ProgramMenuFolder are standard folder
properties for the current user. Root-I File rows
belong to INSTALLDIR; root-P LNK belongs to ProgramMenuFolder. Effective directory
paths, including LocalAppDataFolder/INSTALLPROGRAMS, must match independent OS
known-folder derivation and the fixed Programs/Kwil suffix. Canonical INSTALLDIR
has one trailing `\`; comparisons normalize a trailing separator, not the target.
Resolved paths must be absolute local drive paths, without reparse ancestors,
UNC/device paths, short-name aliases or arbitrary install-location overrides.
The guard checks the current token is non-elevated, current-user, unmanaged MSI
context; elevated, per-machine, managed and other-user execution fail closed.

Every File has a distinct component. Component.Attributes=260 combines the
64-bit bit 256 and RegistryKeyPath bit 4, on BOTH x64 and ARM64. The File component's
KeyPath is its Owner Registry row, never the File identifier. Shell components
use their own individual Registry row as KeyPath. HKCU Registry.Root=1; native
view is mandatory regardless of host process architecture. All components belong
to a single Level-1 Feature `Main`, are local and not advertised. No component
conditions, permanent/shared/transitive flags or automatic advertised repair.
Feature Main has parent NULL, Title `Kwil`, Description `Kwil`, Display 1,
Level 1, Directory_ INSTALLDIR and Attributes 0. Registry-only shell components
use Directory_ INSTALLDIR; a file/Owner component uses its File row's directory.
FeatureComponents contains exactly one Main mapping per component. The guard
checks effective Main action state is local for install/repair and absent for
REMOVE=ALL. Effective Installed presence must agree with independent active
ProductCode registration, and ACTION must equal INSTALL (also for /x uninstall).
An injected Installed or feature state cannot silently skip the authored work.

File keys are `F` plus the six resource ID digits; sequence is ascending file
resource order starting 1. Registry keys are `R` plus six resource ID digits.
File version/language are NULL; Attributes includes compressed 16384. The file
hash inventory is authoritative even for versioned executables. Each FileName
uses a valid short alias and long spelling, `Kdddddd.ext|longname`, where dddddd
are that row's six digits and ext is the first three ASCII alphanumeric extension
characters (lowercase), or no dot/ext if absent. Thus `K000001.exe|kwil.exe` is
valid; do not invent invalid long-only short-name fields or reinterpret
SHORTFILENAMES. Directory DefaultDir is also valid short|long where needed.
No generated short alias may collide with any authored long/short spelling;
reject a collision rather than picking a nondeterministic alias.

Registry.Value uses MSI formatting only for `[INSTALLDIR]` and otherwise literal
data. Backend translates `${I}` to `[INSTALLDIR]`; reject unplanned MSI bracket
escapes or type prefixes. All actual schema-1 values are nonempty REG_SZ and do
not start `#`. Individual value removal is the only authored removal: never
RemoveRegistry or special Name `-`, `+`, `*`, and never recursive key deletion.
Guard requires each declared key/value to match its predecessor before uninstall,
including value type, empty content and key existence. Unknown values/subkeys
are not owned and survive ordinary MSI value removal.

RemoveFile uses NULL FileName and InstallMode=2 for INSTALLDIR (and only genuinely
implicit owned install subdirectories if a later schema adds them), associated
with an installed file component, deepest first. Windows Installer may remove
the directory only if empty. No wildcard or named-file cleanup, parent Programs
removal or ProgramMenuFolder removal is allowed. No CreateFolder table rows:
schema 1 has no true empty-directory resources.

Icon.KwilIcon contains IconPath bytes and is used by ARPPRODUCTICON. Apps &
Features registration is standard per-user MSI registration, not separately
authored HKCU uninstall keys. ARPNOMODIFY/ARPNOREPAIR suppress advertised maintenance
UI; uninstall remains available. CustomAction SetKwilArpLocation has Type=51,
Source=ARPINSTALLLOCATION, Target=`[INSTALLDIR]`; it runs at execute sequence 1010,
condition `1`, after CostFinalize=1000. This is the only property-setter action;
it sets the effective ARPINSTALLLOCATION to the resolved directory rather than
registering the literal format expression. The guard compares it to its own
OS-derived canonical destination; all other immutable properties compare directly
with their database constants. No Environment/PATH/default associations,
services, app-running actions, custom write actions or self-update activation.

Summary Template is exactly `x64;1033` or `Arm64;1033`, PageCount=500,
WordCount=10 (compressed + limited-user package), RevisionNumber=PackageCode.
Media has one embedded cabinet `#kwil.cab`. CAB member names are safe File keys,
not payload filenames. Stage CAB source files under these safe keys inside an
owned bounded work directory; use a DDF whose variable values cannot inject
directives (validate/quote work paths, reject CR/LF, quotes and percent). makecab
does no installation. Backend validates member names, file sizes/hashes and
Media/File mapping after cabinet creation; do not claim native lifecycle from
MSI database parsing alone.

## Deterministic Start Menu shortcut: start-menu-v1

The Start Menu entry is an ordinary nonadvertised Windows Shell Link, installed
as a regular File to the current user's ProgramMenuFolder. This deliberately
avoids variable COM-produced shortcut serialization and automatic MSI advertised
repair. There is no Shortcut table row. The guard compares the exact LNK hash,
so it neither resolves nor executes a link and needs no COM initialization.

Generate exactly 868 bytes, in this order, all multibyte numbers little-endian:

1. A 76-byte ShellLinkHeader: HeaderSize 0x4c; LinkCLSID
   `00021401-0000-0000-C000-000000000046` in ordinary Windows GUID storage order;
   LinkFlags 0x02000200 (`HasExpString | PreferEnvironmentPath`); FileAttributes
   zero; Creation/Access/Write FILETIMEs zero; FileSize zero; IconIndex zero;
   ShowCommand 1; HotKey zero; all Reserved fields zero.
2. One 788-byte EnvironmentVariableDataBlock: BlockSize 0x314, BlockSignature
   0xa0000001, TargetAnsi[260] and TargetUnicode[260 UTF-16LE code units] each
   containing exactly `%LOCALAPPDATA%\Programs\Kwil\kwil.exe` followed by NUL
   and zero padding to their fixed sizes. ASCII encoding for TargetAnsi.
3. A four-byte zero TerminalBlock.

No IDList, LinkInfo, StringData, arguments, working-directory string, tracking,
Darwin, extra icon, property store or additional blocks. The link targets the
GUI exe; its default icon comes from that executable. Environment expansion is
Shell Link behavior at user invocation, not a guard call. Wrapper and backend
independently reproduce these bytes and inventory their hash/size. Disposable
hosted tests must confirm Explorer invocation and icon behavior on Windows
x64/ARM64 before acceptance; this design alone is not proof of shell behavior.

## Guard ABI, dependencies, messages and sequencing

`crates/installer-guard` is an isolated cdylib. Its sole MSI entrypoint is
`KwilPreflightV2`, `unsafe extern "system" fn(u32) -> u32` on both targets.
No unwind crosses FFI. All expected failures are Results mapped to 1603, not
panics. ROOT chooses a compatible abort/static-CRT build configuration; an abort
is a defect, never success evidence. DllMain has no resource scans, allocations
or application initialization. Only system FFI/existing approved permissive
dependencies; no editor/update/GPU/TUI graph, shell commands or script runtime.
Gate both guard architectures' normal AND delay imports against the existing
static-runtime policy, not just the application pair.

Binary row `KwilPreflight` embeds the matching-architecture guard. CustomAction
is exactly Action=KwilPreflight, Type=1, Source=KwilPreflight,
Target=KwilPreflightV2. Type 1 is a synchronous immediate DLL action with checked
return status. No scheduling, impersonation/no-impersonation, async, continue,
deferred or rollback bits. It runs in InstallExecuteSequence at 1490, condition
`1`, after InstallValidate=1400 and before InstallInitialize=1500. It runs even
for basic/quiet UI, maintenance and the old package's nested uninstall.
FindRelatedProducts precedes this guard. Never depend on an InstallUISequence
guard for safety. RemoveExistingProducts is at 1501, immediately after
InstallInitialize and before any other transactional action.

Upgrade table has exactly the three rows in the companion: lower versions have
Attributes=1 (MigrateFeatures, upper bound exclusive) and Remove=ALL; exact equal
has 770 (OnlyDetect + both inclusive bounds); higher has 2 (OnlyDetect, minimum
exclusive). Language bounds are NULL. All use the fixed family. Detection props
are secure but advisory. Never feed command-line KWIL_OLDER directly to removal:
after independent enumeration, guard requires KWIL_OLDER to be exactly the one
verified older ProductCode, or empty when none is eligible. With no registered
predecessor all three detection properties must be empty. During maintenance
and nested uninstall all three must be empty (FindRelatedProducts is not an
authority for those operations). Equal/newer foreign builds already refuse;
they never reach RemoveExistingProducts. A nonempty list, extra GUID, whitespace,
wrong case/format or mismatching result refuses. The guard does not call
MsiSetProperty: successful comparison proves the action property cannot name
an unverified removal target, while the guard remains read-only except for MSI
diagnostic records and transient OS observation bookkeeping.

Return 0 only after every check passes. On a refusal send INSTALLMESSAGE_ERROR
with MSI error 25001: create a three-field record, leave Field 0 NULL, set
Field 1=25001, Field 2=reason/resource, Field 3=instruction;
Error.Message is `Kwil installer preflight: [2]. [3]`. Cap the assembled message
at 1024 UTF-16 units, truncate resource display safely, never truncate checks.
Examples: `foreign-resource I:kwil.exe; move the existing file or choose the
portable ZIP`; `in-use; close Kwil and retry`; `changed-owned-value; restore the
original value or preserve it manually before retrying uninstall`;
`package-mismatch; use the retained original MSI`; `schema-unsupported; use the
matching original installer`. Logging/message failure still returns 1603.
The guard never repairs, writes files/registry, starts apps, shuts down a process,
recursively scans directories/keys, resolves COM shortcuts or contacts a network.

## Guard authority and ownership algorithm

Perform the following in every execute-sequence invocation. Honor every numeric
cap in the companion; missing evidence, permission errors, unsupported types,
unexpected API statuses or exhausted retries refuse before InstallInitialize.
No properties-only ownership or in-use decision is permitted.

1. Open active database with MsiGetActiveDatabase; read typed custom tables,
   immutable Property rows, SummaryInformation, Binary and all authored standard
   rows. Check schema/family/ABI compile-time constants, architecture/PE machine,
   guard hash, complete inventory encoding/hash, BuildID, derived ProductCode,
   fresh-format PackageCode, Summary/Product values and standard row bijections.
   MSI record/string reads use a size query and at most one bounded retry, with
   the null terminator included in allocation limits. Close all handles.
2. Derive current SID/token context and user known folders independently with
   Windows system APIs (SHGetKnownFolderPath for LocalAppData and Programs,
   without creating them). Read-only open each existing filesystem ancestor
   with FILE_READ_ATTRIBUTES, FILE_FLAG_OPEN_REPARSE_POINT and
   FILE_FLAG_BACKUP_SEMANTICS; reject FILE_ATTRIBUTE_REPARSE_POINT, non-directories,
   unresolved/escaped aliases, errors or changed file identity. Check volume and
   final canonical paths, not string prefixes; separators are boundaries.
   When a descendant is absent, check its last existing ancestor then its exact
   absence; do not enumerate siblings. Check each declared file itself without
   following a reparse point, require a regular file, one hardlink, correct type,
   length/hash, and stable file identity/length/mtime before and after reading.
   Special/alternate streams are never authored or deleted.
3. Use MsiEnumRelatedProductsW for the fixed family, at most 16 results plus one
   overflow probe. For each result query MsiGetProductInfoExW in the current SID's
   USERUNMANAGED context (2); reject managed/machine/other-user/conflicting
   registration instead of inferring it is absent. Separately query the active
   ProductCode in that context even when related enumeration is empty. Deduplicate
   that result. At most one matching installed family member is allowed.
   Query INSTALLPROPERTY_LOCALPACKAGE, PACKAGECODE, VERSIONSTRING and
   INSTALLLOCATION; require the expected context and a canonical destination.
   Open the cached LocalPackage with MsiOpenDatabaseW read-only, no repair/re-cache
   or install API. Reject missing/corrupt/unsupported cached packages.
4. Independently validate that cached original's complete schema/table/BuildID/
   ProductCode/PackageCode/family/architecture and expected resource model using
   its own exact schema1/ABI1 or schema2/ABI2 reader. Require registration ProductCode, PackageCode, numeric
   version and canonical destination to match that original. Compare registered
   PACKAGECODE to its Summary revision and KwilPackage.PackageCode. Never use a
   user-supplied sidecar or installed settings as ownership authority. With
   active ProductCode equal to registered ProductCode, active PackageCode must
   also equal the registered original, for repair AND uninstall. Transforms,
   patches and same-ProductCode repacks are refused. A cached original path is
   read only through the MSI registration API; do not invent a cache path.
5. Read every original declared resource directly, with the path checks above.
   For registry ancestors use RegOpenKeyExW with REG_OPTION_OPEN_LINK and
   KEY_QUERY_VALUE | KEY_WOW64_64KEY, then inspect SymbolicLinkValue to reject
   REG_LINK keys at each ancestor; do not follow registry symbolic links.
   Enumerating values/subkeys is unnecessary and forbidden. Check exact declared
   K existence, R type/byte length/content including empty/default values, F
   hashes, and LNK hash. Missing owned F/R/K resources are repairable and may
   remain absent in uninstall or upgrade. A present changed authored file,
   shortcut or value, wrong object type or reparse/link refuses all destructive
   operations, including uninstall. This preserves evidence rather than
   overwriting or deleting user modifications. Unknown siblings/subkeys are
   ignored and never become owned just because a parent directory/key is owned.
6. For every incoming resource, intersect by normalizedIdentity with the
   verified original inventory. A matching resource is owned only with the
   same component identity/schema; existing contents must match the ORIGINAL,
   not the incoming replacement. A newly claimed F, K or R must be absent, even
   during an upgrade. An empty existing key is still an existing foreign K;
   an empty REG_SZ is still a value. An old key does not authorize a newly
   added value in it. Existing unrelated ancestor directories/keys are allowed;
   unknown siblings are preserved. Without registration, ALL preexisting
   claimed resources are foreign, even if their bytes match this package.
   A preexisting `I:kwil.portable` of any type ALWAYS refuses, independently
   of registration or whether this package installs that name. Do not remove it.
7. For maintenance, registration is authority independently of the Upgrade
   detection rows. REMOVE=ALL is allowed only for the verified registered
   original. Repair requires REINSTALL=ALL and REINSTALLMODE=amus with that exact
   original package; other repair/feature/advertised operations refuse. A normal
   /i of an already registered product without this explicit repair request
   refuses with repair instructions. Incoming upgrades require strictly higher
   numeric version, different ProductCode, same family/architecture/destination,
   supported schemas and verified original inventory. Old-package uninstall
   still runs its OWN unconditional guard; UPGRADINGPRODUCTCODE never bypasses
   ownership checks. A rollback script is MSI's transaction restoration, not a
   newly authorized guard write. Downgrades and equal-version distinct builds
   refuse without starting the transaction.
8. Check the registered current pair is not in use as below. Only after this and
   all resource checks, validate the three detection properties against verified
   registration as specified above and return success. Missing registration never authorizes an
   uninstall, and no Active/Installed/REMOVE property can fake registration.

All registry/file handles use minimum read-only access. Roots themselves and
original LocalPackage ancestry are also checked for unsupported redirection;
resource checks do not inspect arbitrary profiles or registry trees. Access
denied is refusal, not absence. Bounded equality comparisons use full content,
never only mtime, length, registry key presence or an ownership marker value.

### Concrete current-pair in-use check

For each existing registered `kwil.exe`/`kwil.com` (checked against the ORIGINAL
inventory), start an observation-only Restart Manager session with
RmStartSession, register those exact two-or-fewer file paths using
RmRegisterResources, then call RmGetList. On ERROR_MORE_DATA allocate at most
256 RM_PROCESS_INFO records and retry once; a second resize, nonzero reboot
reason, any returned process or other error refuses. RmEndSession is mandatory
on every path. No RmShutdown, RmRestart, service enumeration, window messages,
process creation/termination or consent to close applications is involved.
This is transient OS observation bookkeeping, not an installed-resource write.
The guard must link to the system Rstrtmgr DLL; absence is a refusal.

Also open each existing pair member with CreateFileW, desired access
GENERIC_READ | FILE_READ_ATTRIBUTES, share FILE_SHARE_READ only, OPEN_EXISTING,
FILE_FLAG_OPEN_REPARSE_POINT. A sharing violation, lock error or other unexpected
failure refuses; close the successful probe handles before returning. This
probe is read-only and supplements Restart Manager; it does not pretend that
read-only sharing alone detects mapped executable images. If both owned pair
members are missing there is nothing to register/probe; repair may proceed after
the other checks. If one is missing, check the survivor. If a process/path set
changes during the two attempts or file identity checks, refuse and ask to retry.
MSIRESTARTMANAGERCONTROL=Disable and REBOOT=ReallySuppress prohibit MSI from
quietly substituting automatic process shutdown for this fail-before-transaction
policy. Hosted tests must prove running exe AND console/TUI com are caught.

### Boundedness and TOCTOU

The companion's maxima are rejection limits, not throughput targets. Cap all
row counts, string buffers (including API terminators), PE imports, inventory,
paths, byte totals and hashes before allocation/read. Stream files in 64 KiB
chunks; no file-sized buffers. Guard Binary hash/PE evidence can be streamed
with bounded header/section windows, within 16 MiB total. Enforce a 30-second
monotonic preflight deadline between OS calls and chunks, with no sleeps or
retry loops beyond the stated two attempts. OS calls themselves may stall;
this design does not promise in-process cancellation of a stuck Windows API.
Packaging child processes have their own 120-second backend and 60-second
makecab timeout, exact own-PID cleanup and no retries through another wrapper.

These are unsigned local artifacts and snapshot ownership checks. A file, key,
registration or process may change after preflight and before MSI's operation;
read handles are released before transaction writes. No claim of adversarial
race resistance, authenticated cached metadata, continuous exclusion or a
security boundary is made. The guard catches observable collisions and damage
and fails closed on incomplete observations. It does not add an application
hook or a locking process to claim stronger guarantees. Standard MSI failure/
rollback handles later ordinary failures; rollback correctness needs testing.

## Sidecar interface

`OutputPath.manifest.json` is UTF-8/no-BOM JSON with exactly `schema` (integer 2),
`familyGuid`, `componentSchema` (integer 1), `architecture`, `target`, `releaseVersion`, `channel`, `msiVersion`,
`commit`, `buildId`, `inventorySha256`, `productCode`, `packageCode`, `guard`
(same exact shape as input), `inventory` (same complete rows), `msi` (object
with exactly `fileName`, `size`, `sha256`, all strings), `inspection` (object
with exactly `database`, `cabinet`, `staticImports`, each string `passed`),
`signed` (false), `nativeLifecycle` (string `not-run`). Size is canonical decimal;
fileName is OutputPath's basename only. Sidecar contains no own hash and is not
embedded into MSI; MSI contains neither sidecar nor input JSON. Therefore no
checksum cycle exists. Wrapper validates fields/encodings and MSI SHA-256 rather
than accepting a backend exit code alone. Neither sidecar nor BuildID asserts a
signature, an installed lifecycle result, ICE validation or publication.

## Required acceptance, limitations and remaining work

This source package ran only bounded text/JSON/source consistency and hash
checks. No backend or guard exists as a result of these two files. Local later
ROOT work may parse PowerShell, inspect MSIdb tables and extract/inspect CAB/PE
without executing the payload. That is still not native lifecycle acceptance.
Do not install, repair or uninstall anything on the owner's machine for this
work. Full lifecycle runs belong to disposable hosted Windows environments for
both architectures; an emulated source inspection is not an ARM64 lifecycle run.

The companion lists the required hosted cases. Their acceptance includes:

- Clean per-user install yields the exact files, native64 HKCU values, Apps &
  Features entry, GUI Start Menu link and quoted Explorer verb, without persistent
  PATH/default associations or automatic process shutdown.
- Explicit retained-original `msiexec /i <original.msi> REINSTALL=ALL
  REINSTALLMODE=amus` restores missing owned resources. Changed present resources
  refuse; same ProductCode/different PackageCode refuses even via uninstall.
- Higher version removes the verified predecessor immediately after Initialize,
  with BOTH guards running; injected failure after old removal restores the
  original pair, shell values, shortcut and MSI registration via rollback.
- New foreign file, shortcut, key (including empty), value (including empty and
  wrong type) refuses even within an otherwise owned upgrade destination.
  Portable marker, reparse ancestry, registry links, property overrides, running
  exe, running com, corrupt cache and unsupported schema refuse before Initialize.
- Unknown file siblings and unknown values/subkeys survive upgrade, repair and
  uninstall. Settings, recovery and edited documents remain byte-identical.
  A changed authored shell value blocks uninstall rather than deleting it.
- DLL machine/exports/static CRT normal AND delay imports, Summary template/
  PageCount/WordCount, valid short|long names, component attributes/keypaths,
  cabinet member mapping and exact schema1 encoding are independently inspected.

No product-policy question remains open in this frozen interface. Integration
and evidence remain open: ROOT must review and compile the coordinated source,
validate metadata/sidecar retention, standard/custom tables and canonical LNK,
and obtain independent review plus disposable hosted lifecycle evidence.
If Windows emits behavior incompatible
with these fail-closed rules (including the env-target LNK or rollback/cache
registration timing), return a concrete failing case to ROOT for a coordinated
contract revision; do not weaken a guard or silently choose another protocol.

## R2 review corrections

Before InstallInitialize, require both DISABLEROLLBACK and RollbackDisabled to
be empty in effective session properties. Author no DisableRollback action;
LaunchCondition must require NOT RollbackDisabled AND NOT DISABLEROLLBACK, and
the guard independently checks both. This applies to install, repair, upgrade
and nested old-product uninstall. Early predecessor removal requires rollback.

When REINSTALL is empty, REINSTALLMODE may be empty or the normal omus default;
this grants no repair authority. Explicit REINSTALL=ALL still requires exactly
amus and the original registered PackageCode. Reject other modes.

Every File.FileName uses short|long. Directory.DefaultDir follows the exact
rows specified above (including INSTALLDIR=Kwil); use short|long only where
those rows specify it.

The R-data literal-brace prohibition has one exact exception: the fixed braced
family GUID in the schema-1 file Owner value
(familyGuid|architecture|1|file-normalizedIdentity). This is literal data, not
a formatting token. No other dollar/braces expansion is admitted.

## Schema-2 release identity and strict historical readers

The six backend arguments are exactly PayloadDirectory, OutputPath,
ReleaseVersion, Architecture, Commit and IconPath; -Version is no longer accepted.
The archived workspace CargoVersion and application package version must equal
full ReleaseVersion. Wrapper, backend and guard independently derive the mapping.
Typed KwilPackage has exactly 17 columns; its old Version cell becomes
ReleaseVersion, Channel, MsiVersion, in that order. KWIL_RELEASE_VERSION,
KWIL_CHANNEL, ProductName, ProductVersion, schema/ABI and typed identity are bound
to actual database and effective session properties. Recomputing a hash cannot
make a mismapped identity valid. Display names never grant ownership.

KWB2 fields are LP schema, componentSchema, family, architecture, releaseVersion,
channel, msiVersion, commit, target, guardAbi, guardSha256, followed by the existing
raw import count and LP load/DLL pairs, then raw inventory length and KWR1 bytes.
KWP2 fields are LP architecture, releaseVersion, channel, msiVersion and BuildID.
PackageCode remains a fresh UUIDv4 excluded from BuildID. KWR1, KWC1, component
schema/namespace, family GUID, all 29 resources/11 components, Owner bytes,
shortcut bytes, shell values and canonical Programs/Kwil destination are unchanged.
There is one installed family, no beta/stable coexistence. kwil.no-update/manual
updates remain mandatory. The outer manifest is SchemaVersion 2 and records
CargoVersion, ReleaseVersion, Channel and MsiVersion separately.

Active packages accept ONLY exact schema2/component1/ABI2 with V2 export.
Cached originals route by exact typed layout to a separate schema1/component1/ABI1
reader (15 columns, sole KwilPreflightV1) or schema2/component1/ABI2 reader
(17 columns, sole KwilPreflightV2). Legacy KWB1 is LP 1,1,family,architecture,
numeric Version,commit,target,1,guardSha256 then the existing imports/inventory;
legacy KWP1 is LP architecture,numeric Version,BuildID. Legacy has no invented
channel or semantic release version. Each reader retains exact standard-table,
canonical hash, resource and export checks. New active packages never fall back
to legacy decoding. Existing original V1 guards still run for their own nested
uninstall. Registration must agree with the cached original's ProductCode,
PackageCode, numeric MSI version and canonical current-user destination.

A legacy numeric 0.1.0 installation refuses every beta downgrade and the new
stable equal 0.1.0; explicitly uninstall using its retained original first.
Exact original-package repair/uninstall and same-architecture strictly-higher
replacement remain the only paths. A repack's fresh PackageCode does not grant
maintenance authority. No resource, rollback, path, token, in-use or preservation
check is weakened by this identity migration.

## Qualification remains held

This is source preparation for a future beta.2 setup, not an installer build or
acceptance. The existing beta.1 raw tag at 904e5d5 is immutable. Release/publish
workflow wiring, updater changes, dependencies and KW-66 native fixes are outside
this package. NativeCommandStartup run 40312316490 at 908515e failed on both
architectures with 0xC0000142, no sentinel and complete cleanup. No further native
experiment is authorized here. A High-IL restricted candidate cannot establish
ordinary Medium-IL user acceptance. Retain all five lifecycle gaps and the
harness's unconditional qualification failure. Only the owner publishes.

Authored controls must cover exact release boundaries/order, independently
encoded KWB2/KWP2 vectors, mapping/display/session tampering even after rehash,
strict schema/ABI/export disagreement, unchanged component/Owner bytes, strict
legacy cache maintenance/equal/downgrade/architecture/package identity and every
existing guard safety control. ROOT owns fail-first and focused execution;
no authored control is claimed run by this source package.

## Canonical API and fixture migration map

- Wrapper Get-DistReleaseIdentity and backend Get-KwilReleaseIdentity return
  full releaseVersion, channel, mapped msiVersion and visible productName.
  Get-DistNames uses the full release version. Initialize-DistInstallerInput
  accepts ReleaseVersion and emits exact schema2/component1/ABI2 metadata.
  Get-DistInstallerIdentity/Get-KwilBuildId write KWB2; product names use KWP2.
- Guard release_identity returns ReleaseIdentity. Package keeps numeric
  msi_version and optional ReleaseIdentity; None means exact legacy identity,
  never an invented stable channel. expected_product/build_bytes select the
  exact historical encoding. read is active schema2 only; read_cached selects
  an exact layout; read_legacy_cached is the strict historical schema1 reader.
  The adapter's field-count ceiling is 17 solely because KwilPackage grew by
  two typed cells. Every table still requires its exact schema/order/types.
- test-windows-msi.ps1 moves its writer fixture from numeric 1.2.3 to full
  0.1.0-beta.2 with numeric 0.0.2. Guard export fixture bytes change V1 to V2,
  keeping length and all PE safety mutations. Original negative CLI spellings,
  integer/duplicate JSON, CAB/schema, timeout/receipt and resource controls stay.
  The independent List-of-bytes reference now encodes every KWB2 field.
- test-dist.ps1 replaces writer numeric-domain positives with stable/beta
  canonical boundaries; the legacy numeric parser's full range remains covered
  in the guard fixtures. Existing invalid numeric inputs remain invalid writer
  versions. Schema/ABI expectations become 2; component/resource expectations
  stay 1/29/11. Integer-representation symmetry remains exact-value checked;
  the actual JSON reader accepts only schema=2/abi=2/componentSchema=1 by member.
- The lifecycle script's migration is mechanical: strict schema2 sidecar read,
  new argument/field names, derived full ProductName and mapped registration
  version. Its higher fixture increments the accepted beta ordinal, or selects
  stable after beta.65535; stable has no supported higher release and refuses.
  This remains a metadata/changed-LICENSES fixture, not a new compiled release
  identity or permission to alter Cargo/application provenance. Its five gap
  strings and unconditional full-qualification failure remain exact.
- Rust legacy package/database fixtures need only the explicit optional legacy
  identity, numeric field name, ABI-aware evidence interface and historical
  reader entry point. Their original numeric 1.2.3/1.2.4 comparisons and guard
  resource/PE safety inputs remain legacy controls. Added schema2 fixtures cover
  current mapping, active-versus-cached isolation, rehash resistance, display and
  effective-session binding, unchanged Owner/components and original authority.
  Stream fixtures retain their exact legacy PE path and all bounds oracles.

The companion's schema2-beta10 empty-inventory vector fixes the complete KWB2
and KWP2 bytes independently of production encoders; it is not an installable
resource set. Its proposed BuildID is `cb271f49ebab129086e7586a18105aa74f59b9f270f2391412a7a607b3162324`
and ProductCode `{BCAF26CE-222B-5CFC-841D-1DAD00D90B20}`. These are authored
controls only. No test, compiler, interop method, MSI or hosted run was executed.
