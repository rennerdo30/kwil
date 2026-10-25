#requires -Version 7.2
<#
KW-59: disposable GitHub-hosted Windows token preflight, NOT installer acceptance.
Authoring/parse-only work is separate from running this file: Add-Type below is
only for ROOT's later admitted hosted experiment. Never run it on the owner PC.

One limited linked counterpart, or ONE LUA_TOKEN|DISABLE_MAX_PRIVILEGE restricted
own-primary candidate. Neither a restriction nor elevation type Default proves
non-elevation. Every candidate and the suspended/actual child must be measured.
No scheduler, credentials, alternate account, UAC/group/desktop ACL changes,
profile loading, MSI, editor, network, persistent environment or fallback launch.

Interop reference: cached Windows SDK 10.0.26100.0 winnt.h,
securitybaseapi.h, processthreadsapi.h; default platform alignment on x64/ARM64.
No token bytes/handles/privileges or environment dumps go into public receipts.
Token measurements serialize only the bounded facts enumerated in Facts.
NativeCommandStartup is a dispatch-only diagnostic alternative, never a worker
proof: read parent window-station/desktop DACL access, then launch exactly one
native SystemRoot/System32/cmd.exe /d /q /c "exit /b 73". No PowerShell fallback.
Parent DACL observations do not prove child attachment, MIC or DLL startup.

90s monotonic internal budget starts BEFORE admission/Add-Type. Cleanup reserves
one cumulative 10s deadline across all exact-handle reaps (at most 5s per wait),
evidence reserves 20s; child lifetime is at most 20s
and capped again immediately before creation. The timer never performs file IO
or PowerShell callbacks. Outer workflow 120s step fence handles a stalled parent
API/receipt; OS APIs themselves cannot be forcibly cancelled in-process.
Qualification needs exit 0 AND agreeing driver/worker receipts; a lone PASS file
is insufficient. No redirected pipes are created, so there is no pipe drain.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('x64','arm64')][string]$Architecture,
    [Parameter(Mandatory)][string]$EvidenceDirectory,
    [ValidateSet('Driver','Worker')][string]$Mode = 'Driver',
    [string]$OwnerNonce = '',
    [ValidateSet('PowerShellProof','NativeCommandStartup',IgnoreCase=$false)][string]$Purpose = 'PowerShellProof'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Clock = [Diagnostics.Stopwatch]::StartNew()
$script:BudgetMs = 90000L
$script:CleanupReserveMs = 10000L
$script:EvidenceReserveMs = 20000L
$script:Lease = $null
$script:Cleanup = $null
$script:ParentAccess = $null

function Assert-TokenCase([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Token preflight FAIL/design-fork: $Message" }
}

function Assert-PlainPath([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            Assert-TokenCase (((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -eq 0) "reparse ancestry: $cursor"
        }
        $next = [IO.Path]::GetDirectoryName($cursor)
        if ($next -eq $cursor) { break }
        $cursor = $next
    }
}

function Assert-HostedTokenContext {
    # Reused lifecycle admission pattern, plus an explicit disposable-run marker.
    # Every mode passes this BEFORE filesystem writes or Add-Type/native access.
    Assert-TokenCase ($IsWindows -and $env:GITHUB_ACTIONS -ceq 'true') 'Windows GITHUB_ACTIONS=true required'
    Assert-TokenCase ($env:RUNNER_ENVIRONMENT -ceq 'github-hosted' -and $env:RUNNER_OS -ceq 'Windows') 'disposable GitHub-hosted Windows only'
    Assert-TokenCase ($env:GITHUB_REPOSITORY -ceq 'rennerdo30/kwil') 'unexpected repository'
    Assert-TokenCase ($env:GITHUB_RUN_ID -cmatch '^[0-9]{1,24}$' -and $env:GITHUB_RUN_ATTEMPT -cmatch '^[0-9]{1,8}$') 'run identity absent'
    Assert-TokenCase ($env:GITHUB_SHA -cmatch '^[0-9a-f]{40}$') 'exact source commit absent'
    Assert-TokenCase ($env:KWIL_INSTALLER_TOKEN_DISPOSABLE -ceq "${env:GITHUB_RUN_ID}:${env:GITHUB_RUN_ATTEMPT}") 'explicit disposable marker absent'
    Assert-TokenCase ($env:RUNNER_ARCH -ceq $(if ($Architecture -ceq 'x64') { 'X64' } else { 'ARM64' })) 'runner architecture mismatch'
    $native = if ($Architecture -ceq 'x64') { 'X64' } else { 'Arm64' }
    Assert-TokenCase ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() -ceq $native) 'OS architecture mismatch'
    Assert-TokenCase ([Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString() -ceq $native) 'native PowerShell required'
    if ($Purpose -ceq 'NativeCommandStartup') {
        Assert-TokenCase ($Mode -ceq 'Driver' -and $env:GITHUB_EVENT_NAME -ceq 'workflow_dispatch') 'native diagnostic requires explicit driver workflow dispatch'
    }
    Assert-TokenCase ($env:GITHUB_WORKSPACE -and [IO.Path]::IsPathFullyQualified($env:GITHUB_WORKSPACE)) 'absolute workspace required'
    Assert-TokenCase ([IO.Path]::IsPathFullyQualified($EvidenceDirectory)) 'absolute evidence path required'
    $root = [IO.Path]::GetFullPath($env:GITHUB_WORKSPACE).TrimEnd('\')
    $evidence = [IO.Path]::GetFullPath($EvidenceDirectory).TrimEnd('\')
    # Exact one owned directory, never arbitrary profile/temp or another checkout.
    Assert-TokenCase ($evidence -ceq (Join-Path $root '.token-preflight')) 'evidence must be workspace/.token-preflight'
    $script:Repository = $root
    $script:EvidenceDirectory = $evidence
    Assert-PlainPath $root
    Assert-PlainPath $evidence
    Assert-TokenCase ([IO.Path]::GetFullPath($PSCommandPath) -ceq (Join-Path $root 'scripts/test-windows-installer-token.ps1')) 'script is not the checked-out source'
    Assert-PlainPath $PSCommandPath
    Assert-PlainPath (Join-Path $root '.github/workflows/windows-installer-token.yml')
    if ($Mode -ceq 'Worker') {
        Assert-TokenCase ($OwnerNonce -cmatch '^[0-9a-f]{32}$' -and $env:KWIL_TOKEN_WORKER_NONCE -ceq $OwnerNonce) 'worker lacks driver correlation marker'
        Assert-TokenCase (Test-Path -LiteralPath $evidence -PathType Container) 'worker evidence directory absent'
    } else {
        Assert-TokenCase ($OwnerNonce.Length -eq 0 -and -not $env:KWIL_TOKEN_WORKER_NONCE) 'driver cannot be a nested worker'
        Assert-TokenCase (-not (Test-Path -LiteralPath $evidence)) 'evidence directory already exists; no reuse/overwrite'
    }
}

function Get-LaunchAdmission([long]$RemainingMs) {
    $available = $RemainingMs - $script:CleanupReserveMs - $script:EvidenceReserveMs
    Assert-TokenCase ($available -ge 1) 'no lifetime after cleanup/evidence reserve; refuse before creation'
    [int][Math]::Min(20000L, $available)
}

function Write-BoundedJson([string]$Name, $Value) {
    Assert-TokenCase ($script:Clock.ElapsedMilliseconds -lt $script:BudgetMs) 'deadline before evidence write'
    Assert-TokenCase ($Name -cmatch '^[a-z0-9-]+[.]json$') 'invalid receipt name'
    $text = ($Value | ConvertTo-Json -Depth 10 -Compress) + "`n"
    Assert-TokenCase ([Text.Encoding]::UTF8.GetByteCount($text) -le 32768) 'receipt exceeds 32KiB'
    $path = Join-Path $EvidenceDirectory $Name
    # CreateNew avoids accepting a stale/partially overwritten proof.
    $stream = [IO.File]::Open($path, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($text)
        $stream.Write($bytes, 0, $bytes.Length)
    } finally { $stream.Dispose() }
    Assert-TokenCase ($script:Clock.ElapsedMilliseconds -lt $script:BudgetMs) 'deadline during evidence write; receipt alone is not acceptance'
}

function Get-SourceIdentity {
    $scriptHash = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $workflowHash = (Get-FileHash -LiteralPath (Join-Path $Repository '.github/workflows/windows-installer-token.yml') -Algorithm SHA256).Hash.ToLowerInvariant()
    [ordered]@{repository=$env:GITHUB_REPOSITORY;commit=$env:GITHUB_SHA;run=$env:GITHUB_RUN_ID;attempt=$env:GITHUB_RUN_ATTEMPT
        architecture=$Architecture;scriptSha256=$scriptHash;workflowSha256=$workflowHash}
}

function Assert-TokenFactsAgree($Expected, $Actual, [string]$Phase) {
    foreach ($field in @('UserSid','AuthenticationId','LogonSid','SessionId','TokenType','TokenElevation','TokenElevationType','IntegritySid','AdminSidPresent','AdminSidAttributes','EffectiveAdmin','LinkedTokenOutcome')) {
        Assert-TokenCase ($Actual.$field -ceq $Expected.$field) "$Phase token mismatch: $field"
    }
}

function Read-BoundedProof([string]$Path) {
    Assert-PlainPath $Path
    $stream = [IO.File]::Open($Path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
    try {
        Assert-TokenCase ($stream.Length -ge 1 -and $stream.Length -le 32768) 'worker proof size bound'
        # Fixed allocation/read cap, even if a future writer grows the file.
        $bytes = [byte[]]::new(32768)
        $count = 0
        while ($count -lt $bytes.Length) {
            $read = $stream.Read($bytes,$count,$bytes.Length-$count)
            if ($read -eq 0) { break }
            $count += $read
        }
        Assert-TokenCase ($stream.ReadByte() -eq -1) 'worker proof grew beyond cap'
        [Text.UTF8Encoding]::new($false,$true).GetString($bytes,0,$count) | ConvertFrom-Json
    } finally { $stream.Dispose() }
}

function Get-PublicFailure($Failure) {
    $exception = $Failure.Exception
    $diagnostics = [Collections.Generic.List[string]]::new()
    $nativeError = $null
    for ($depth=0; $null -ne $exception -and $depth -lt 8; $depth++) {
        if ($exception -is [ComponentModel.Win32Exception]) { $nativeError=$exception.NativeErrorCode }
        foreach ($key in @('launchOwnership','launchCleanup','cleanupFailure','finalCleanup','finalReceiptFailure')) {
            if ($exception.Data.Contains($key)) {
                $text = "$key=$($exception.Data[$key])"
                $diagnostics.Add($text.Substring(0,[Math]::Min(2048,$text.Length)))
            }
        }
        $exception = $exception.InnerException
    }
    $message = [string]$Failure.Exception.Message
    [ordered]@{message=$message.Substring(0,[Math]::Min(2048,$message.Length));nativeErrorCode=$nativeError;diagnostics=$diagnostics.ToArray()}
}

function Write-EvidenceManifest($Identity, [string]$Status) {
    $rows = @('owned-process.json','worker-proof.json','worker-result.json','driver-result.json') | ForEach-Object {
        $path = Join-Path $EvidenceDirectory $_
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Assert-PlainPath $path
            $length = (Get-Item -LiteralPath $path).Length
            Assert-TokenCase ($length -le 32768) 'evidence artifact exceeds cap'
            [ordered]@{name=$_;bytes=$length;sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()}
        } else { [ordered]@{name=$_;state='absent'} }
    }
    Write-BoundedJson 'evidence-manifest.json' ([ordered]@{schema=1;identity=$Identity;status=$Status;artifacts=@($rows);selfHash='excluded';installerQualification='not-run'})
}

function Write-NativeEvidenceManifest($Identity, [string]$Status) {
    $rows = @('parent-access.json','owned-process.json','worker-proof.json','worker-result.json','driver-result.json') | ForEach-Object {
        $path = Join-Path $EvidenceDirectory $_
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Assert-PlainPath $path
            $length = (Get-Item -LiteralPath $path).Length
            Assert-TokenCase ($length -le 32768) 'diagnostic artifact exceeds cap'
            [ordered]@{name=$_;bytes=$length;sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()}
        } else { [ordered]@{name=$_;state='absent'} }
    }
    Write-BoundedJson 'evidence-manifest.json' ([ordered]@{schema=2;kind='native-command-startup-diagnostic';purpose=$Purpose
        identity=$Identity;status=$Status;artifacts=@($rows);selfHash='excluded';tokenPreflight='not-run';installerQualification='not-run'})
}

function Assert-NativeCommandImage([string]$Path) {
    # Bounded PE reads only. Native parent/OS agreement is already an admission
    # gate; this independently rejects an unexpected System32 image machine.
    $stream = [IO.File]::Open($Path,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)
    try {
        $dos = [byte[]]::new(64)
        Assert-TokenCase ($stream.Read($dos,0,$dos.Length) -eq $dos.Length) 'native command short DOS header'
        Assert-TokenCase ($dos[0] -eq 0x4d -and $dos[1] -eq 0x5a) 'native command DOS signature'
        $offset = [BitConverter]::ToInt32($dos,60)
        Assert-TokenCase ($offset -ge 64 -and $offset -le 1048576 -and [long]$offset + 26 -le $stream.Length) 'native command PE offset bound'
        $null = $stream.Seek($offset,[IO.SeekOrigin]::Begin)
        $pe = [byte[]]::new(26)
        Assert-TokenCase ($stream.Read($pe,0,$pe.Length) -eq $pe.Length) 'native command short PE header'
        $machine = [BitConverter]::ToUInt16($pe,4)
        $expected = if ($Architecture -ceq 'x64') { 0x8664 } else { 0xaa64 }
        Assert-TokenCase ([BitConverter]::ToUInt32($pe,0) -eq 0x4550 -and $machine -eq $expected -and [BitConverter]::ToUInt16($pe,20) -ge 2 -and [BitConverter]::ToUInt16($pe,24) -eq 0x20b) ('native command PE architecture/PE32+ mismatch; machine=0x{0:x4} expected=0x{1:x4}' -f $machine,$expected)
        [uint16]$machine
    } finally { $stream.Dispose() }
}

function Invoke-OwnedProof([scriptblock]$Create, [scriptblock]$Receipt, [scriptblock]$Work, [scriptblock]$Close) {
    # Same exception/cleanup seam for the real child and source-authored mocks.
    # Create returns only after native ownership + timer are armed. No receipt IO
    # can bypass the finally, and a receipt failure never masks the primary error.
    $owner = $null; $primary = $null; $result = $null; $cleanupError = $null
    try {
        $owner = & $Create
        & $Receipt $owner
        $result = & $Work $owner
    } catch { $primary = $_ }
    finally {
        if ($null -ne $owner) {
            try { & $Close $owner } catch { $cleanupError = $_ }
        }
    }
    if ($primary) {
        if ($cleanupError) { $primary.Exception.Data['cleanupFailure'] = $cleanupError.Exception.Message }
        throw $primary
    }
    if ($cleanupError) { throw $cleanupError }
    return $result
}

Assert-HostedTokenContext
if ($Mode -ceq 'Driver') { $null = [IO.Directory]::CreateDirectory($EvidenceDirectory) }

$identity = Get-SourceIdentity
$report = [ordered]@{schema=1;kind='same-sid-token-preflight';status='FAIL/design-fork';identity=$identity
    mode=$Mode;processPid=$PID;installerQualification='not-run';uacPolicy='not-inspected';budgetMilliseconds=$script:BudgetMs
    baseline=$null;selection=$null;suspended=$null;actual=$null;cleanup=$null;oracles=$null;error=$null}
if ($Purpose -ceq 'NativeCommandStartup') {
    $report.schema = 2
    $report.kind = 'native-command-startup-diagnostic'
    $report['purpose'] = $Purpose
    $report['tokenPreflight'] = 'not-run'
    $report['workerProof'] = 'not-observed'
    $report['workerResult'] = 'not-observed'
    $report['parentAccess'] = $null
    $report['nativeCommand'] = [ordered]@{image='SystemRoot/System32/cmd.exe';commandId='cmd-d-q-c-exit-b-73'
        expectedExitCode=73;exitCode=$null;reached=$false;machine=$null;childAttachment='not-observed'
        startupDesktop='null';creationFlags='0x08000404';environmentValues='private';environmentNames=$null;environmentUtf16Bytes=$null}
}
$primaryError = $null
$current = $null
$candidate = $null
try {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

public static class KwilTokenProof {
    const uint QUERY=0x0008, DUPLICATE=0x0002, ASSIGN_PRIMARY=0x0001;
    const uint DENY_ONLY=0x10, ENABLED=0x4, LOGON_ID=0xc0000000;
    const int MaxTokenBytes=65536, MaxGroups=256;
    [StructLayout(LayoutKind.Sequential)] struct Luid { public uint Low; public int High;
        public override string ToString() { return unchecked((uint)High).ToString("x8")+":"+Low.ToString("x8"); } }
    [StructLayout(LayoutKind.Sequential)] struct Statistics {
        public Luid TokenId, AuthenticationId; public long Expiration;
        public int TokenType, ImpersonationLevel; public uint DynamicCharged, DynamicAvailable, GroupCount, PrivilegeCount;
        public Luid ModifiedId;
    }
    [StructLayout(LayoutKind.Sequential)] struct SidAttributes { public IntPtr Sid; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] struct GroupsOne { public uint Count; public SidAttributes First; }
    [StructLayout(LayoutKind.Sequential)] struct SI {
        public uint cb; public IntPtr reserved, desktop, title;
        public uint x,y,width,height,xChars,yChars,fill,flags; public ushort show,reservedBytes;
        public IntPtr reserved2,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] struct PI { public IntPtr process,thread; public uint pid,tid; }
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentThread();
    [DllImport("kernel32.dll")] public static extern uint GetCurrentProcessId();
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint GetProcessId(IntPtr process);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr process,uint access,out IntPtr token);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenThreadToken(IntPtr thread,uint access,bool openAsSelf,out IntPtr token);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,IntPtr data,uint bytes,out uint needed);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool ConvertSidToStringSidW(IntPtr sid,out IntPtr text);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool ConvertStringSidToSidW(string text,out IntPtr sid);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool DuplicateTokenEx(IntPtr token,uint access,IntPtr attributes,int level,int type,out IntPtr duplicate);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool CheckTokenMembership(IntPtr token,IntPtr sid,out bool member);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool CreateRestrictedToken(IntPtr token,uint flags,uint disableCount,ref SidAttributes disable,uint deleteCount,IntPtr delete,uint restrictCount,IntPtr restrict,out IntPtr result);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessAsUserW(IntPtr token,string app,StringBuilder command,IntPtr pa,IntPtr ta,bool inherit,uint flags,IntPtr environment,string cwd,ref SI startup,out PI process);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint WaitForSingleObject(IntPtr handle,uint milliseconds);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr process,uint code);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr process,out uint code);

    static Exception Error(string api) { return Error(api,Marshal.GetLastWin32Error()); }
    static Exception Error(string api,int e) {
        return new Win32Exception(e,api+" error="+e+(e==1314?"; FAIL/design-fork: privilege not held, no retry":"; refusal, no retry")); }
    static void Require(bool value,string reason) { if(!value) throw new InvalidOperationException("FAIL/design-fork: "+reason); }
    static void Close(ref IntPtr handle) { if(handle!=IntPtr.Zero) {
        if(!CloseHandle(handle)) throw Error("CloseHandle"); handle=IntPtr.Zero; } }
    public static void CheckLayout() {
        Require(IntPtr.Size==8 && Marshal.SizeOf(typeof(SI))==104 && Marshal.SizeOf(typeof(PI))==24 &&
            Marshal.SizeOf(typeof(Statistics))==56 && Marshal.SizeOf(typeof(SidAttributes))==16 &&
            Marshal.OffsetOf(typeof(GroupsOne),"First").ToInt32()==8,"SDK x64/ARM64 layout mismatch");
    }
    public sealed class Token : IDisposable {
        internal IntPtr Handle;
        internal Token(IntPtr handle) { Require(handle!=IntPtr.Zero,"null token handle"); Handle=handle; }
        public void Dispose() { Close(ref Handle); }
    }
    internal delegate bool InfoQuery(IntPtr token,int kind,IntPtr data,uint bytes,out uint needed,out int error);
    static bool NativeInfoQuery(IntPtr token,int kind,IntPtr data,uint bytes,out uint needed,out int error) {
        bool ok=GetTokenInformation(token,kind,data,bytes,out needed);
        error=Marshal.GetLastWin32Error(); // Capture before allocation, validation or disposal.
        return ok;
    }
    static uint FixedInfoBytes(int kind) {
        // Cached SDK 10.0.26100.0 winnt.h: TOKEN_ELEVATION is one DWORD;
        // TOKEN_ELEVATION_TYPE and TokenSessionId are 4 bytes, STATISTICS is
        // 56 bytes, LINKED_TOKEN is one HANDLE (8 on the admitted x64/ARM64 ABI).
        // USER / GROUPS / MANDATORY_LABEL / LOGON_SID include variable SID data.
        switch(kind) {
            case 10: return 56;
            case 12: case 18: case 20: return 4;
            case 19: return 8;
            case 1: case 2: case 25: case 28: return 0;
            default: throw new InvalidOperationException("unsupported token information class="+kind);
        }
    }
    sealed class Info : IDisposable {
        internal IntPtr Data; internal uint Length;
        internal Info(IntPtr token,int kind) : this(token,kind,NativeInfoQuery) { }
        internal Info(IntPtr token,int kind,InfoQuery query) {
            uint n=FixedInfoBytes(kind);
            if(n==0) {
                // ROOT's hosted run 40183935867 failed class 20's zero-size
                // probe with ERROR_BAD_LENGTH (24), before any worker launch.
                // ROOT reference: MS Learn GetTokenInformation, recorded in
                // kwil-token-preflight-native-r1/FAILURE.md (required length,
                // not a universal promise of ERROR_INSUFFICIENT_BUFFER).
                // GetTokenInformation does not promise universal error 122:
                // fixed classes bypass probing, not validation. Variable
                // classes retain the strict expected sizing-error contract.
                int error; bool ok=query(token,kind,IntPtr.Zero,0,out n,out error);
                Require(!ok,"GetTokenInformation probe unexpectedly succeeded class="+kind+" length="+n);
                uint minimum=(kind==1 || kind==25)?16U:4U;
                if(error!=122 || n<minimum || n>MaxTokenBytes)
                    throw Error("GetTokenInformation size class="+kind+" length="+n,error);
            }
            Data=Marshal.AllocHGlobal(checked((int)n));
            try {
                int error;
                if(!query(token,kind,Data,n,out Length,out error))
                    throw Error("GetTokenInformation class="+kind+" capacity="+n+" length="+Length,error);
                Require(Length>0 && Length<=n,"token information size changed");
                Require(Length==n,"GetTokenInformation inconsistent/short return class="+kind+" expected="+n+" length="+Length);
            }
            catch { Dispose(); throw; }
        }
        internal void AtLeast(int bytes) { Require(Length>=bytes,"short token information"); }
        public void Dispose() { if(Data!=IntPtr.Zero) { Marshal.FreeHGlobal(Data); Data=IntPtr.Zero; } }
    }
    sealed class MockInfoQuery {
        internal int Kind, Calls, Probes, Reads, ProbeError=122, ReadError;
        internal uint ProbeLength=64, ReadLength=64, Capacity;
        internal bool ProbeSuccess, ReadSuccess=true;
        internal bool Query(IntPtr token,int kind,IntPtr data,uint bytes,out uint needed,out int error) {
            Require(token==new IntPtr(101) && kind==Kind,"mock token information identity changed");
            Calls++;
            if(bytes==0) {
                Require(data==IntPtr.Zero,"mock non-null zero-size probe"); Probes++;
                needed=ProbeLength; error=ProbeError; return ProbeSuccess;
            }
            Require(data!=IntPtr.Zero && bytes<=MaxTokenBytes,"mock read allocation/capacity bound");
            Reads++; Capacity=bytes; needed=ReadLength; error=ReadError; return ReadSuccess;
        }
    }
    static void RefuseInfo(MockInfoQuery mock,int nativeError,int probes,int reads) {
        bool refused=false;
        try { using(var info=new Info(new IntPtr(101),mock.Kind,mock.Query)) { } }
        catch(Win32Exception e) { Require(nativeError>=0 && e.NativeErrorCode==nativeError,"mock lost original Win32 error"); refused=true; }
        catch(InvalidOperationException) { Require(nativeError<0,"mock substituted a policy error for the original Win32 error"); refused=true; }
        Require(refused && mock.Probes==probes && mock.Reads==reads && mock.Calls==probes+reads,
            "mock buffer failure accepted or retried");
    }
    public static void CheckInfoBufferOracles() {
        // Injected calls only: no token/native APIs. A zero-size fixed-class
        // probe returns the observed error 24; every fixed read must bypass it.
        var badLengthProbe=new MockInfoQuery { Kind=20,ProbeError=24,ProbeLength=4 };
        uint needed; int error;
        Require(!badLengthProbe.Query(new IntPtr(101),20,IntPtr.Zero,0,out needed,out error) && needed==4 && error==24,
            "fixed zero-size probe fixture did not reproduce the hosted error");
        // SDK expectations are independent of the production size selector.
        int[,] fixedCases={ {10,56},{12,4},{18,4},{19,8},{20,4} };
        for(int row=0;row<fixedCases.GetLength(0);row++) {
            int kind=fixedCases[row,0]; uint size=checked((uint)fixedCases[row,1]);
            var fixedRead=new MockInfoQuery { Kind=kind,ProbeError=24,ProbeLength=size,ReadLength=size,ReadError=5 };
            using(var info=new Info(new IntPtr(101),kind,fixedRead.Query)) {
                Require(info.Length==size && fixedRead.Calls==1 && fixedRead.Probes==0 && fixedRead.Reads==1 && fixedRead.Capacity==size,
                    "fixed ABI read was not exactly one correctly sized call");
            }
            // Last-error is meaningful only on API failure; a successful read
            // above deliberately leaves stale error 5. Failed reads preserve it.
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadSuccess=false,ReadLength=size,ReadError=24 },24,0,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadSuccess=false,ReadLength=size,ReadError=5 },5,0,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=0 },-1,0,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=size-1 },-1,0,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=size+1 },-1,0,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=65537 },-1,0,1);
        }
        foreach(int kind in new int[] { 1,2,25,28 }) {
            var variable=new MockInfoQuery { Kind=kind };
            using(var info=new Info(new IntPtr(101),kind,variable.Query)) {
                Require(info.Length==64 && variable.Probes==1 && variable.Reads==1 && variable.Calls==2 && variable.Capacity==64,
                    "variable information did not use one bounded probe and read");
            }
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeError=24 },24,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeError=5 },5,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeSuccess=true },-1,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeLength=0 },122,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeLength=65537 },122,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ProbeLength=3 },122,1,0);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=0 },-1,1,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=63 },-1,1,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadLength=65 },-1,1,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadSuccess=false,ReadError=5 },5,1,1);
            RefuseInfo(new MockInfoQuery { Kind=kind,ReadSuccess=false,ReadLength=128,ReadError=122 },122,1,1);
        }
        var cap=new MockInfoQuery { Kind=2,ProbeLength=65536,ReadLength=65536 };
        using(var info=new Info(new IntPtr(101),2,cap.Query)) {
            Require(info.Length==65536 && cap.Capacity==65536 && cap.Calls==2,"64KiB variable information bound changed");
        }
        RefuseInfo(new MockInfoQuery { Kind=1,ProbeLength=15 },122,1,0);
        RefuseInfo(new MockInfoQuery { Kind=25,ProbeLength=15 },122,1,0);
    }
    static uint U32(IntPtr token,int kind) { using(var p=new Info(token,kind)) { p.AtLeast(4); return unchecked((uint)Marshal.ReadInt32(p.Data)); } }
    static string Sid(IntPtr pointer) {
        IntPtr text;
        if(!ConvertSidToStringSidW(pointer,out text)) throw Error("ConvertSidToStringSidW");
        try { int length=0; while(length<184 && Marshal.ReadInt16(text,length*2)!=0) length++;
            Require(length>0 && length<184,"SID text bound"); return Marshal.PtrToStringUni(text,length); }
        finally { LocalFree(text); }
    }
    static string ReadGroups(IntPtr token,int kind,out uint adminAttributes,out bool adminPresent) {
        adminAttributes=0; adminPresent=false; string logon=null; int logonCount=0;
        using(var p=new Info(token,kind)) {
            p.AtLeast(4); uint count=unchecked((uint)Marshal.ReadInt32(p.Data));
            Require(count<=MaxGroups,"group count bound");
            int offset=Marshal.OffsetOf(typeof(GroupsOne),"First").ToInt32(), stride=Marshal.SizeOf(typeof(SidAttributes));
            Require((long)offset+(long)count*stride<=p.Length,"group array bound");
            for(int i=0;i<count;i++) {
                var row=Marshal.PtrToStructure<SidAttributes>(IntPtr.Add(p.Data,offset+i*stride));
                string sid=Sid(row.Sid);
                if(sid=="S-1-5-32-544") { Require(!adminPresent,"duplicate Administrators SID"); adminPresent=true; adminAttributes=row.Attributes; }
                if((row.Attributes & LOGON_ID)==LOGON_ID) { logon=sid; logonCount++; }
            }
        }
        Require(logonCount==1,"exactly one logon SID required"); return logon;
    }
    public sealed class Facts {
        public string UserSid, AuthenticationId, LogonSid, IntegritySid, LinkedTokenOutcome;
        public uint SessionId, TokenElevation, TokenElevationType, AdminSidAttributes;
        public bool AdminSidPresent, EffectiveAdmin;
        public int TokenType;
    }
    static Token Linked(IntPtr token,uint elevationType) {
        // Default documents absence of a split/link, NOT absence of elevation.
        if(elevationType==1) return null;
        Require(elevationType==2 || elevationType==3,"unknown elevation type");
        using(var p=new Info(token,19)) { p.AtLeast(IntPtr.Size); return new Token(Marshal.ReadIntPtr(p.Data)); }
    }
    public static Facts Measure(Token token) { return MeasureHandle(token.Handle); }
    static Facts MeasureHandle(IntPtr token) {
        var f=new Facts();
        using(var p=new Info(token,1)) { p.AtLeast(Marshal.SizeOf(typeof(SidAttributes))); f.UserSid=Sid(Marshal.PtrToStructure<SidAttributes>(p.Data).Sid); }
        using(var p=new Info(token,10)) { p.AtLeast(Marshal.SizeOf(typeof(Statistics))); var s=Marshal.PtrToStructure<Statistics>(p.Data);
            f.AuthenticationId=s.AuthenticationId.ToString(); f.TokenType=s.TokenType;
            Require(s.GroupCount<=MaxGroups && s.PrivilegeCount<=256,"statistics count bound"); }
        f.SessionId=U32(token,12); f.TokenElevationType=U32(token,18); f.TokenElevation=U32(token,20);
        Require(f.TokenElevation<=1 && f.TokenElevationType>=1 && f.TokenElevationType<=3,"token elevation enum invalid");
        f.LogonSid=ReadGroups(token,2,out f.AdminSidAttributes,out f.AdminSidPresent);
        uint unused; bool absent;
        Require(ReadGroups(token,28,out unused,out absent)==f.LogonSid,"TokenLogonSid disagrees with TokenGroups");
        using(var p=new Info(token,25)) { p.AtLeast(Marshal.SizeOf(typeof(SidAttributes))); f.IntegritySid=Sid(Marshal.PtrToStructure<SidAttributes>(p.Data).Sid); }
        IntPtr admin=IntPtr.Zero, duplicate=IntPtr.Zero;
        try {
            if(!ConvertStringSidToSidW("S-1-5-32-544",out admin)) throw Error("ConvertStringSidToSidW");
            // CheckTokenMembership requires an impersonation token, not the
            // primary handle and not a null/current-thread implicit token.
            if(!DuplicateTokenEx(token,QUERY,IntPtr.Zero,2,2,out duplicate)) throw Error("DuplicateTokenEx membership");
            if(!CheckTokenMembership(duplicate,admin,out f.EffectiveAdmin)) throw Error("CheckTokenMembership");
        } finally { try { Close(ref duplicate); } finally { if(admin!=IntPtr.Zero) LocalFree(admin); } }
        using(var linked=Linked(token,f.TokenElevationType)) { f.LinkedTokenOutcome=linked==null?"no-link-type-Default":"linked-handle-opened-and-closed"; }
        return f;
    }
    public static Token Current(bool launchRights) {
        IntPtr thread;
        if(OpenThreadToken(GetCurrentThread(),QUERY,true,out thread)) {
            Close(ref thread); throw new InvalidOperationException("FAIL/design-fork: unexpected effective thread impersonation");
        }
        if(Marshal.GetLastWin32Error()!=1008) throw Error("OpenThreadToken");
        IntPtr primary;
        uint access=QUERY|DUPLICATE|(launchRights?ASSIGN_PRIMARY:0);
        if(!OpenProcessToken(GetCurrentProcess(),access,out primary)) throw Error("OpenProcessToken current");
        return new Token(primary);
    }
    public static void SameIdentity(Facts original,Facts candidate) {
        Require(candidate.UserSid==original.UserSid,"user SID mismatch");
        Require(candidate.AuthenticationId==original.AuthenticationId,"authentication/logon LUID mismatch");
        Require(candidate.LogonSid==original.LogonSid,"logon SID mismatch");
        Require(candidate.SessionId==original.SessionId,"session mismatch");
    }
    public static void Standard(Facts f) {
        Require(f.UserSid!=null && f.UserSid.StartsWith("S-1-") && f.UserSid!="S-1-5-18" && f.UserSid!="S-1-5-19" && f.UserSid!="S-1-5-20" && !f.UserSid.StartsWith("S-1-5-80-"),"service/invalid SID");
        Require(f.SessionId!=0,"session zero"); Require(f.TokenType==1,"primary token required");
        Require(f.TokenElevation==0,"TokenElevation is nonzero");
        Require(f.TokenElevationType==1 || f.TokenElevationType==3,"TokenElevationType Full/unknown");
        Require(!f.EffectiveAdmin,"CheckTokenMembership reports effective Administrators");
        Require(!f.AdminSidPresent || ((f.AdminSidAttributes & DENY_ONLY)!=0 && (f.AdminSidAttributes & ENABLED)==0),"Administrators SID is not deny-only");
    }
    public sealed class Selection {
        public string Path, LinkedOutcome; public Facts LinkedFacts, CandidateFacts;
    }
    public static Token Select(Token original,Facts baseline,Selection evidence) {
        Token candidate=null;
        try {
            using(var linked=Linked(original.Handle,baseline.TokenElevationType)) {
                evidence.LinkedOutcome=linked==null?"no-link-type-Default":"linked-counterpart-inspected";
                if(linked!=null) {
                    evidence.LinkedFacts=Measure(linked); SameIdentity(baseline,evidence.LinkedFacts);
                    if(evidence.LinkedFacts.TokenElevationType==3) {
                        Standard(evidence.LinkedFacts);
                        IntPtr primary;
                        if(!DuplicateTokenEx(linked.Handle,QUERY|DUPLICATE|ASSIGN_PRIMARY,IntPtr.Zero,2,1,out primary)) throw Error("DuplicateTokenEx linked primary");
                        candidate=new Token(primary); evidence.Path="verified-limited-linked-primary";
                    } else {
                        Require(baseline.TokenElevationType==3 && evidence.LinkedFacts.TokenElevationType==2,"unexpected linked counterpart; no fallback");
                        evidence.LinkedOutcome="full-counterpart-not-a-limited-candidate";
                    }
                }
            }
            if(candidate==null) {
                evidence.Path="one-restricted-own-primary-LUA_TOKEN-DISABLE_MAX_PRIVILEGE";
                IntPtr admin=IntPtr.Zero;
                try {
                    if(!ConvertStringSidToSidW("S-1-5-32-544",out admin)) throw Error("ConvertStringSidToSidW restriction");
                    var disable=new SidAttributes { Sid=admin, Attributes=0 }; IntPtr restricted;
                    // 0x4 LUA_TOKEN | 0x1 DISABLE_MAX_PRIVILEGE; no SANDBOX_INERT.
                    // Explicit SidsToDisable makes Administrators deny-only.
                    if(!CreateRestrictedToken(original.Handle,0x5,1,ref disable,0,IntPtr.Zero,0,IntPtr.Zero,out restricted)) throw Error("CreateRestrictedToken");
                    candidate=new Token(restricted);
                } finally { if(admin!=IntPtr.Zero) LocalFree(admin); }
            }
            evidence.CandidateFacts=Measure(candidate);
            SameIdentity(baseline,evidence.CandidateFacts); Standard(evidence.CandidateFacts);
            Token selected=candidate; candidate=null; return selected;
        } finally { if(candidate!=null) candidate.Dispose(); }
    }
    public static void OwnedIdentity(uint recorded,uint observed) {
        Require(recorded!=0 && recorded==observed,"foreign PID/handle identity; no termination grant");
    }
    public sealed class Cleanup {
        public uint Pid, ThreadId, ExitCode;
        public bool DeadlineExpired, TerminationAttempted, Exited, ProcessClosed, ThreadClosed, Resumed;
        public string Diagnostic="";
        public bool Success { get { return Exited && ProcessClosed && ThreadClosed && Diagnostic.Length==0; } }
    }
    // This clock and process-call seam is also used by the pure late-watchdog
    // oracle. No mock opens a token, starts a timer or calls a native API.
    internal interface ProcessCalls {
        uint Pid(IntPtr process);
        uint Wait(IntPtr process,uint milliseconds);
        void Terminate(IntPtr process);
        uint ExitCode(IntPtr process);
        void Close(ref IntPtr handle);
    }
    sealed class NativeProcessCalls : ProcessCalls {
        public uint Pid(IntPtr process) { uint pid=GetProcessId(process); if(pid==0) throw Error("GetProcessId owned"); return pid; }
        public uint Wait(IntPtr process,uint milliseconds) { return WaitForSingleObject(process,milliseconds); }
        public void Terminate(IntPtr process) { if(!TerminateProcess(process,0xe059)) throw Error("TerminateProcess"); }
        public uint ExitCode(IntPtr process) { uint code; if(!GetExitCodeProcess(process,out code)) throw Error("GetExitCodeProcess"); return code; }
        public void Close(ref IntPtr handle) { KwilTokenProof.Close(ref handle); }
    }
    sealed class CleanupDeadline {
        readonly Func<long> elapsed; readonly int reserve; long deadline; bool begun;
        internal CleanupDeadline(Func<long> elapsedMilliseconds,int lifetimeMs,int reserveMs) {
            Require(reserveMs==10000,"cleanup reserve must remain 10s");
            elapsed=elapsedMilliseconds; reserve=reserveMs; deadline=checked((long)lifetimeMs+reserveMs);
        }
        internal void Begin() {
            if(begun) return;
            // Early refusal gets at most the same reserve; a delayed watchdog
            // cannot move the end later than the original lifetime + reserve.
            deadline=Math.Min(deadline,checked(elapsed()+reserve)); begun=true;
        }
        internal long RemainingMilliseconds { get { return Math.Max(0,deadline-elapsed()); } }
        internal uint WaitMilliseconds { get { return checked((uint)Math.Min(5000L,RemainingMilliseconds)); } }
    }
    public sealed class Lease {
        readonly object gate=new object(); readonly Func<long> elapsed; readonly int lifetime;
        readonly CleanupDeadline cleanupDeadline; readonly ProcessCalls calls;
        PI owned; Timer watchdog; bool closed; readonly Cleanup result=new Cleanup();
        internal Lease(Func<long> elapsedMilliseconds,int lifetimeMs,int cleanupMs,ProcessCalls processCalls) {
            elapsed=elapsedMilliseconds; lifetime=lifetimeMs; calls=processCalls;
            cleanupDeadline=new CleanupDeadline(elapsedMilliseconds,lifetimeMs,cleanupMs);
        }
        public uint Pid { get { return result.Pid; } }
        public uint ThreadId { get { return result.ThreadId; } }
        public int LifetimeMilliseconds { get { return lifetime; } }
        public long RemainingMilliseconds { get { return Math.Max(0,lifetime-elapsed()); } }
        public Cleanup Snapshot { get { lock(gate) { return result; } } }
        void Note(string s) { if(result.Diagnostic.Length<2048) result.Diagnostic+=(result.Diagnostic.Length==0?"":"; ")+s; }
        void Verify() { Require(!closed && owned.process!=IntPtr.Zero,"closed/unowned process");
            OwnedIdentity(owned.pid,calls.Pid(owned.process)); }
        void Stop() {
            cleanupDeadline.Begin();
            Verify(); uint status=calls.Wait(owned.process,0);
            if(status==258) {
                if(cleanupDeadline.RemainingMilliseconds>0) {
                    result.TerminationAttempted=true;
                    try { calls.Terminate(owned.process); } catch(Exception e) { Note(e.Message); }
                    // Recompute after termination: it too may have consumed the
                    // reserve. Every later Close shares this exact deadline.
                    status=calls.Wait(owned.process,cleanupDeadline.WaitMilliseconds);
                } else Note("cumulative cleanup deadline exhausted");
            }
            result.Exited=status==0;
            if(!result.Exited) Note("owned handle not reaped; wait="+status);
            else { try { result.ExitCode=calls.ExitCode(owned.process); } catch(Exception e) { Note(e.Message); } }
        }
        void Adopt(PI process) { owned=process; result.Pid=process.pid; result.ThreadId=process.tid; }
        void WatchdogTick() {
            lock(gate) {
                if(closed) return;
                // No file/receipt/runspace code under the ownership lock.
                result.DeadlineExpired=true;
                try { Stop(); } catch(Exception e) { Note(e.Message); }
            }
        }
        void Arm() {
            lock(gate) {
                Verify();
                watchdog=new Timer(_ => WatchdogTick(),null,checked((int)Math.Max(1,RemainingMilliseconds)),Timeout.Infinite);
            }
        }
        public Facts InspectSuspended() {
            lock(gate) {
                Verify(); Require(!result.Resumed && !result.DeadlineExpired && RemainingMilliseconds>0,"suspended inspection deadline");
                IntPtr token;
                if(!OpenProcessToken(owned.process,QUERY|DUPLICATE,out token)) throw Error("OpenProcessToken suspended child");
                using(var t=new Token(token)) return Measure(t);
            }
        }
        public void Resume() {
            lock(gate) {
                Verify(); Require(!result.Resumed && !result.DeadlineExpired && RemainingMilliseconds>0,"resume deadline/state");
                uint previous=ResumeThread(owned.thread);
                if(previous==0xffffffff) throw Error("ResumeThread");
                Require(previous==1,"unexpected suspend count"); result.Resumed=true;
            }
        }
        public uint Wait() {
            // One bounded wait; timer has its independently owned exact handle.
            IntPtr process; uint ms;
            lock(gate) { Verify(); process=owned.process; ms=checked((uint)Math.Max(1,RemainingMilliseconds)); }
            uint status=calls.Wait(process,ms);
            lock(gate) {
                Require(status==0 && !result.DeadlineExpired && RemainingMilliseconds>0,"proof worker timeout/deadline");
                return calls.ExitCode(owned.process);
            }
        }
        public Cleanup Close() {
            lock(gate) {
                if(closed) return result;
                try { if(!result.Exited) Stop(); } catch(Exception e) { Note(e.Message); }
                if(result.Exited) {
                    try { if(!result.ThreadClosed) { calls.Close(ref owned.thread); result.ThreadClosed=true; } } catch(Exception e) { Note(e.Message); }
                    try { if(!result.ProcessClosed) { calls.Close(ref owned.process); result.ProcessClosed=true; } } catch(Exception e) { Note(e.Message); }
                    closed=result.ThreadClosed && result.ProcessClosed;
                    if(closed && watchdog!=null) { watchdog.Dispose(); watchdog=null; }
                }
                // Unreaped ownership stays live, with the timer and handles;
                // reporting failure never claims another PID as a substitute.
                return result;
            }
        }
        sealed class MockProcessCalls : ProcessCalls {
            internal long Now, Waited; internal int Terminations, Closes;
            void Exact(IntPtr process) { Require(process==new IntPtr(1),"mock changed original process handle"); }
            public uint Pid(IntPtr process) { Exact(process); return 101; }
            public uint Wait(IntPtr process,uint milliseconds) {
                Exact(process); Require(milliseconds<=5000,"mock reap exceeded per-wait bound");
                Now=checked(Now+milliseconds); Waited+=milliseconds; return 258;
            }
            public void Terminate(IntPtr process) { Exact(process); Terminations++; Now+=250; }
            public uint ExitCode(IntPtr process) { throw new InvalidOperationException("mock unexited process queried"); }
            public void Close(ref IntPtr handle) { Closes++; throw new InvalidOperationException("mock unreaped handle closed"); }
        }
        public static void CheckCleanupDeadlineOracle() {
            foreach(int delay in new int[] { 500,7000,11000 }) {
                var fake=new MockProcessCalls { Now=20000L+delay };
                var lease=new Lease(() => fake.Now,20000,10000,fake);
                lease.Adopt(new PI { process=new IntPtr(1),thread=new IntPtr(2),pid=101,tid=102 });
                long start=fake.Now;
                // The watchdog gets the ownership lock late. The same Stop and
                // Close code then services Invoke-OwnedProof and outer finally.
                lease.WatchdogTick();
                long afterWatchdog=fake.Now;
                Cleanup invokeCleanup=lease.Close();
                long afterInvoke=fake.Now;
                Cleanup finalCleanup=lease.Close();
                Require(fake.Now-start==Math.Max(0,10000-delay),"late watchdog + both cleanups exceeded cumulative reserve");
                Require(fake.Waited+250L*fake.Terminations==fake.Now-start,"mock cleanup time was not fully accounted");
                if(delay==500) Require(afterWatchdog==25750 && afterInvoke==30000,"mock did not exercise both bounded reap waits");
                Require(fake.Now==afterInvoke,"final cleanup acquired a new deadline after failure");
                lease.Close(); // Even another failure path cannot renew the reserve.
                Require(fake.Now==afterInvoke && fake.Closes==0,"repeated cleanup renewed budget or released unreaped ownership");
                Require(!invokeCleanup.Success && !finalCleanup.Success && finalCleanup.DeadlineExpired &&
                    !finalCleanup.Exited && !finalCleanup.ProcessClosed && !finalCleanup.ThreadClosed &&
                    finalCleanup.Pid==101 && finalCleanup.ThreadId==102 && finalCleanup.Diagnostic.Length>0 &&
                    lease.owned.process==new IntPtr(1) && lease.owned.thread==new IntPtr(2),"failure diagnostics lost original handles/identity");
            }
            // An early failure starts one shorter window; a later watchdog and
            // final cleanup must not move its end out to lifetime + reserve.
            var early=new MockProcessCalls { Now=1000 };
            var failed=new Lease(() => early.Now,20000,10000,early);
            failed.Adopt(new PI { process=new IntPtr(1),thread=new IntPtr(2),pid=101,tid=102 });
            failed.Close(); failed.WatchdogTick(); failed.Close();
            Require(early.Now==11000 && early.Waited+250L*early.Terminations==10000 && early.Closes==0,
                "early cleanup failure renewed its cumulative deadline");
        }
        internal static Lease Start(Token token,string app,string command,string environment,string cwd,int lifetimeMs,int cleanupMs) {
            Require(lifetimeMs>0 && lifetimeMs<=20000,"child lifetime invalid");
            var watch=Stopwatch.StartNew(); var lease=new Lease(() => watch.ElapsedMilliseconds,lifetimeMs,cleanupMs,new NativeProcessCalls());
            IntPtr block=IntPtr.Zero;
            try {
                block=Marshal.StringToHGlobalUni(environment);
                var startup=new SI { cb=checked((uint)Marshal.SizeOf(typeof(SI))),flags=1,show=0 };
                PI process;
                // SUSPENDED | UNICODE_ENVIRONMENT | CREATE_NO_WINDOW.
                // No desktop override, handle inheritance, stdio pipes or shell.
                if(!CreateProcessAsUserW(token.Handle,app,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,false,0x08000404,block,cwd,ref startup,out process)) throw Error("CreateProcessAsUserW");
                // No allocations/receipt IO between returned handles and adoption.
                lease.Adopt(process);
                lease.Arm();
                return lease;
            } catch(Exception original) {
                if(lease.owned.process!=IntPtr.Zero) {
                    // Keep the exact owning object reachable even if arming or
                    // launch cleanup failed before Launch could return it.
                    // Public diagnostics whitelist strings, never this object.
                    original.Data["launchLease"]=lease;
                    original.Data["launchOwnership"]="pid="+lease.owned.pid+" threadId="+lease.owned.tid+" createdSuspended=true";
                    try { var cleanup=lease.Close(); if(!cleanup.Success) original.Data["launchCleanup"]=cleanup.Diagnostic; }
                    catch(Exception cleanup) { original.Data["launchCleanup"]=cleanup.Message; }
                }
                throw;
            } finally { if(block!=IntPtr.Zero) Marshal.FreeHGlobal(block); }
        }
    }
    public static Lease Launch(Token token,string app,string command,string environment,string cwd,int lifetimeMs,int cleanupMs) {
        return Lease.Start(token,app,command,environment,cwd,lifetimeMs,cleanupMs);
    }

    // Diagnostic-only interop. No Set*, OpenDesktop, profile or impersonation
    // attachment API. Get*Desktop/WindowStation handles are borrowed, never closed.
    [StructLayout(LayoutKind.Sequential)] struct GenericMapping {
        public uint Read, Write, Execute, All;
    }
    [StructLayout(LayoutKind.Sequential)] struct AccessLuidAttributes { public Luid Id; public uint Attributes; }
    [StructLayout(LayoutKind.Sequential)] struct PrivilegeSetOne { public uint Count, Control; public AccessLuidAttributes First; }
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("user32.dll",SetLastError=true)] static extern IntPtr GetProcessWindowStation();
    [DllImport("user32.dll",SetLastError=true)] static extern IntPtr GetThreadDesktop(uint thread);
    [DllImport("user32.dll",SetLastError=true)] static extern bool GetUserObjectSecurity(IntPtr handle,ref uint requested,[Out] byte[] descriptor,uint length,out uint needed);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool GetUserObjectInformationW(IntPtr handle,int index,[Out] byte[] data,uint length,out uint needed);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool AccessCheck([In] byte[] descriptor,IntPtr token,uint desired,ref GenericMapping mapping,[Out] byte[] privileges,ref uint privilegeBytes,out uint granted,out bool access);
    const uint MaximumAllowed=0x02000000;
    const int MaxDescriptorBytes=65536, MaxNameBytes=512, MaxPrivilegeBytes=65536;
    const int MaxAclEntries=4096, MaxAccessPrivileges=256;
    // SDK winnt.h GENERIC_MAPPING (16 bytes, DWORD offsets 0/4/8/12),
    // PRIVILEGE_SET header 8 + N*12 LUID_AND_ATTRIBUTES bytes. No pointer fields.
    // Cached SDK winnt.h: LUID_AND_ATTRIBUTES 10283, SID 10336, ACL 11059,
    // ACE_HEADER 11089, SECURITY_DESCRIPTOR_RELATIVE 11570, PRIVILEGE_SET 11721.
    // Object mappings: MS Learn desktop/window-station security access-rights
    // tables; WinUser.h 1372..1380 / 1552..1564, winnt.h 10220..10230.
    // https://learn.microsoft.com/en-us/windows/win32/winstation/desktop-security-and-access-rights
    // https://learn.microsoft.com/en-us/windows/win32/winstation/window-station-security-and-access-rights
    // AccessCheck API success and AccessStatus are separate; only a failed API
    // makes last-error meaningful. DACL results do not include MIC or establish
    // which desktop the child later attaches to with startup.desktop == null.
    static GenericMapping ObjectMapping(bool station) {
        return station ? new GenericMapping { Read=0x00020303,Write=0x0002001c,Execute=0x00020060,All=0x000f037f }
            : new GenericMapping { Read=0x00020041,Write=0x000200be,Execute=0x00020100,All=0x000f01ff };
    }
    interface AccessCalls {
        bool Duplicate(IntPtr primary,out IntPtr owned,out int error);
        IntPtr Borrow(bool station);
        bool Read(IntPtr borrowed,bool name,byte[] data,uint capacity,out uint needed,out int error);
        bool Check(byte[] descriptor,IntPtr token,uint desired,ref GenericMapping mapping,byte[] privileges,ref uint bytes,out uint granted,out bool access,out int error);
        void Close(ref IntPtr owned);
    }
    sealed class NativeAccessCalls : AccessCalls {
        public bool Duplicate(IntPtr primary,out IntPtr owned,out int error) {
            // Only TOKEN_QUERY on the new SecurityImpersonation impersonation
            // token. No TOKEN_IMPERSONATE, ASSIGN_PRIMARY or thread attachment.
            IntPtr created;
            bool ok=DuplicateTokenEx(primary,QUERY,IntPtr.Zero,2,2,out created);
            error=ok?0:Marshal.GetLastWin32Error();
            owned=ok?created:IntPtr.Zero; // A failed API's undefined output is not an ownership grant.
            return ok;
        }
        public IntPtr Borrow(bool station) {
            IntPtr handle=station?GetProcessWindowStation():GetThreadDesktop(GetCurrentThreadId());
            if(handle==IntPtr.Zero) throw Error(station?"GetProcessWindowStation":"GetThreadDesktop parent");
            return handle;
        }
        public bool Read(IntPtr borrowed,bool name,byte[] data,uint capacity,out uint needed,out int error) {
            uint requested=7; // OWNER | GROUP | DACL, never SACL or label requests.
            bool ok=name?GetUserObjectInformationW(borrowed,2,data,capacity,out needed)
                :GetUserObjectSecurity(borrowed,ref requested,data,capacity,out needed);
            error=ok?0:Marshal.GetLastWin32Error(); return ok;
        }
        public bool Check(byte[] descriptor,IntPtr token,uint desired,ref GenericMapping mapping,byte[] privileges,ref uint bytes,out uint granted,out bool access,out int error) {
            bool ok=AccessCheck(descriptor,token,desired,ref mapping,privileges,ref bytes,out granted,out access);
            error=ok?0:Marshal.GetLastWin32Error(); return ok;
        }
        public void Close(ref IntPtr owned) { KwilTokenProof.Close(ref owned); }
    }
    static byte[] ReadObject(AccessCalls calls,IntPtr borrowed,bool name) {
        uint needed; int error; string api=name?"GetUserObjectInformationW name":"GetUserObjectSecurity";
        bool ok=calls.Read(borrowed,name,null,0,out needed,out error);
        Require(!ok,api+" sizing unexpectedly succeeded");
        if(error!=122) throw Error(api+" size",error);
        int cap=name?MaxNameBytes:MaxDescriptorBytes, minimum=name?2:20;
        Require(needed>=minimum && needed<=cap && (!name || needed%2==0),api+" sizing bound");
        byte[] data=new byte[checked((int)needed)]; uint returned;
        if(!calls.Read(borrowed,name,data,needed,out returned,out error)) throw Error(api+" read",error);
        Require(returned==needed,api+" changed/short read; no retry");
        return data;
    }
    static string ObjectName(byte[] data) {
        Require(data.Length>=2 && data.Length<=MaxNameBytes && data.Length%2==0 &&
            data[data.Length-2]==0 && data[data.Length-1]==0,"object name terminator/bound");
        string name=new UnicodeEncoding(false,false,true).GetString(data,0,data.Length-2);
        Require(name.Length>0 && name.Length<=255,"object name character bound");
        foreach(char c in name) Require(!char.IsControl(c),"object name control/embedded terminator");
        return name;
    }
    static void DescriptorSid(byte[] data,uint offset) {
        Require(offset>=20 && offset%4==0 && (long)offset+8<=data.Length,"descriptor SID header offset");
        int at=checked((int)offset); int count=data[at+1];
        Require(data[at]==1 && count<=15 && (long)at+8+4L*count<=data.Length,"descriptor SID extent");
    }
    static void DescriptorBounds(byte[] data) {
        Require(data.Length>=20 && data.Length<=MaxDescriptorBytes && data[0]==1,"descriptor header bound/revision");
        ushort control=BitConverter.ToUInt16(data,2);
        Require((control&0x8000)!=0 && BitConverter.ToUInt32(data,12)==0,"descriptor must be self-relative without SACL");
        DescriptorSid(data,BitConverter.ToUInt32(data,4)); DescriptorSid(data,BitConverter.ToUInt32(data,8));
        uint dacl=BitConverter.ToUInt32(data,16);
        if(dacl==0) return; // A null/absent DACL is an observation, not rewritten.
        Require((control&4)!=0 && dacl>=20 && dacl%4==0 && (long)dacl+8<=data.Length,"descriptor DACL header offset");
        int start=checked((int)dacl), size=BitConverter.ToUInt16(data,start+2), count=BitConverter.ToUInt16(data,start+4);
        Require((data[start]==2 || data[start]==4) && size>=8 && (long)start+size<=data.Length &&
            count<=MaxAclEntries && count<=(size-8)/4,"descriptor ACL size/count bound");
        int at=start+8, end=start+size;
        for(int i=0;i<count;i++) {
            Require((long)at+4<=end,"descriptor ACE header bound");
            int bytes=BitConverter.ToUInt16(data,at+2);
            Require(bytes>=4 && bytes%4==0 && (long)at+bytes<=end,"descriptor ACE extent"); at+=bytes;
        }
    }
    public sealed class AccessOutcome {
        public uint RequestedAccess=MaximumAllowed;
        public bool ApiSucceeded;
        public int? LastError;
        public bool? AccessStatus;
        public uint? GrantedAccess;
    }
    public sealed class ObjectAccess {
        public string Label, Name;
        public AccessOutcome Baseline, Candidate;
    }
    static AccessOutcome CheckAccess(AccessCalls calls,byte[] descriptor,IntPtr token,bool station) {
        var mapping=ObjectMapping(station); byte[] privileges=new byte[MaxPrivilegeBytes];
        uint bytes=MaxPrivilegeBytes, granted; bool access; int error;
        bool ok=calls.Check(descriptor,token,MaximumAllowed,ref mapping,privileges,ref bytes,out granted,out access,out error);
        var result=new AccessOutcome { ApiSucceeded=ok,LastError=ok?(int?)null:error };
        if(!ok) return result; // AccessStatus/GrantedAccess/length are undefined on API failure.
        Require(bytes>=8 && bytes<=MaxPrivilegeBytes,"AccessCheck privilege-set length bound");
        uint count=BitConverter.ToUInt32(privileges,0);
        Require(count<=MaxAccessPrivileges && 8L+12L*count<=bytes,"AccessCheck privilege-set extent");
        Require((BitConverter.ToUInt32(privileges,4)&~1U)==0,"AccessCheck privilege-set control");
        result.AccessStatus=access;
        if(access) {
            Require((granted&~ObjectMapping(station).All)==0,"AccessCheck returned unmapped/unknown rights");
            result.GrantedAccess=granted;
        }
        return result;
    }
    public sealed class ParentAccess {
        readonly AccessCalls calls;
        IntPtr baselineCopy, candidateCopy; bool started;
        public string Scope="parent-context-DACL-only-not-child-attachment-or-MIC";
        public ObjectAccess[] Objects={new ObjectAccess { Label="parent-window-station" },new ObjectAccess { Label="parent-thread-desktop" }};
        public string CleanupOutcome="not-started";
        public bool BaselineTokenClosed { get { return baselineCopy==IntPtr.Zero; } }
        public bool CandidateTokenClosed { get { return candidateCopy==IntPtr.Zero; } }
        public bool TokensClosed { get { return baselineCopy==IntPtr.Zero && candidateCopy==IntPtr.Zero; } }
        public ParentAccess() { calls=new NativeAccessCalls(); }
        ParentAccess(AccessCalls injected) { calls=injected; }
        public void Measure(Token baseline,Token candidate) { MeasureHandles(baseline.Handle,candidate.Handle); }
        void MeasureHandles(IntPtr baseline,IntPtr candidate) {
            Require(!started,"parent access observation cannot retry"); started=true;
            int error;
            // Successful acquisitions go directly to their owner; a failed
            // second duplicate leaves the first reachable for exact cleanup.
            // Public receipts cannot serialize these private fields.
            if(!calls.Duplicate(baseline,out baselineCopy,out error)) throw Error("DuplicateTokenEx baseline AccessCheck",error);
            Require(baselineCopy!=IntPtr.Zero,"null baseline AccessCheck token");
            if(!calls.Duplicate(candidate,out candidateCopy,out error)) throw Error("DuplicateTokenEx candidate AccessCheck",error);
            Require(candidateCopy!=IntPtr.Zero,"null candidate AccessCheck token");
            for(int i=0;i<2;i++) {
                bool station=i==0; IntPtr borrowed=calls.Borrow(station);
                Require(borrowed!=IntPtr.Zero,"null borrowed user object");
                Objects[i].Name=ObjectName(ReadObject(calls,borrowed,true));
                byte[] descriptor=ReadObject(calls,borrowed,false); DescriptorBounds(descriptor);
                Objects[i].Baseline=CheckAccess(calls,descriptor,baselineCopy,station);
                if(!Objects[i].Baseline.ApiSucceeded) throw Error("AccessCheck baseline",Objects[i].Baseline.LastError.Value);
                Objects[i].Candidate=CheckAccess(calls,descriptor,candidateCopy,station);
                if(!Objects[i].Candidate.ApiSucceeded) throw Error("AccessCheck candidate",Objects[i].Candidate.LastError.Value);
            }
        }
        public void Close() {
            Exception first=null;
            try { if(baselineCopy!=IntPtr.Zero) calls.Close(ref baselineCopy); } catch(Exception e) { first=e; }
            try { if(candidateCopy!=IntPtr.Zero) calls.Close(ref candidateCopy); } catch(Exception e) { if(first==null) first=e; }
            CleanupOutcome=TokensClosed?"closed":"unresolved-owned-token";
            if(first!=null) throw first; // Both owned handles were attempted; never touch borrowed handles.
        }
        sealed class MockAccessCalls : AccessCalls {
            internal int Duplicates, Acquired, Closed, Reads, Checks, FailDuplicate, FailRead;
            internal bool ProbeSuccess, ReadSuccess=true, AccessSuccess=true, AccessAllowed=true, FailClose;
            internal uint ProbeLength=40, ReturnLength=40, PrivilegeLength=8, PrivilegeCount, PrivilegeControl;
            internal uint? GrantedOverride;
            internal uint NameProbeLength=10, NameReturnLength=10;
            internal int ProbeError=122, ReadError=5, AccessError=5;
            public bool Duplicate(IntPtr primary,out IntPtr owned,out int error) {
                Duplicates++; Require(primary==new IntPtr(Duplicates),"mock changed input token");
                bool ok=Duplicates!=FailDuplicate;
                owned=ok?new IntPtr(300+Duplicates):IntPtr.Zero;
                if(owned!=IntPtr.Zero) Acquired++; error=5; return ok;
            }
            public IntPtr Borrow(bool station) { return new IntPtr(station?401:402); }
            public bool Read(IntPtr borrowed,bool name,byte[] data,uint capacity,out uint needed,out int error) {
                Require(borrowed==new IntPtr(401) || borrowed==new IntPtr(402),"mock borrowed object changed"); Reads++;
                if(Reads==FailRead) { needed=0; error=ReadError; return false; }
                if(capacity==0) { Require(data==null,"mock sizing pointer"); needed=name?NameProbeLength:ProbeLength; error=ProbeError; return ProbeSuccess; }
                Require(data!=null && data.Length==capacity && capacity<=(name?MaxNameBytes:MaxDescriptorBytes),"mock read capacity");
                needed=name?NameReturnLength:ReturnLength; error=ReadError;
                if(!ReadSuccess) return false;
                byte[] source=name?Encoding.Unicode.GetBytes("mock\0"):MockDescriptor();
                Array.Copy(source,data,Math.Min(source.Length,data.Length)); return true;
            }
            public bool Check(byte[] descriptor,IntPtr token,uint desired,ref GenericMapping mapping,byte[] privileges,ref uint bytes,out uint granted,out bool access,out int error) {
                Require(token==new IntPtr(301) || token==new IntPtr(302),"mock access token changed");
                Require(desired==0x02000000 && bytes==65536 && privileges.Length==65536,"mock access request/capacity");
                bool station=Checks<2; Checks++;
                Require(mapping.Read==(station?0x20303U:0x20041U) && mapping.Write==(station?0x2001cU:0x200beU) &&
                    mapping.Execute==(station?0x20060U:0x20100U) && mapping.All==(station?0xf037fU:0xf01ffU),"mock object mapping changed");
                bytes=PrivilegeLength; Array.Copy(BitConverter.GetBytes(PrivilegeCount),privileges,4);
                Array.Copy(BitConverter.GetBytes(PrivilegeControl),0,privileges,4,4);
                granted=GrantedOverride.HasValue?GrantedOverride.Value:mapping.Read;
                access=AccessAllowed; error=AccessError; return AccessSuccess;
            }
            public void Close(ref IntPtr owned) {
                Require(owned==new IntPtr(301) || owned==new IntPtr(302),"borrowed/foreign handle close");
                if(FailClose && owned==new IntPtr(301)) throw Error("mock owned token close",6);
                Closed++; owned=IntPtr.Zero;
            }
        }
        static byte[] MockDescriptor() {
            var data=new byte[40]; data[0]=1; data[2]=4; data[3]=0x80;
            Array.Copy(BitConverter.GetBytes(20U),0,data,4,4); Array.Copy(BitConverter.GetBytes(20U),0,data,8,4);
            Array.Copy(BitConverter.GetBytes(32U),0,data,16,4); data[20]=1; data[21]=1; data[27]=5;
            data[32]=2; data[34]=8; return data;
        }
        static void Refuse(MockAccessCalls mock,int error,int reads,int acquired) {
            var owner=new ParentAccess(mock); bool refused=false;
            try { owner.MeasureHandles(new IntPtr(1),new IntPtr(2)); }
            catch(Win32Exception e) { Require(e.NativeErrorCode==error,"access oracle lost Win32 error"); refused=true; }
            catch(InvalidOperationException) { Require(error<0,"access oracle replaced Win32 error"); refused=true; }
            finally { owner.Close(); }
            Require(refused && mock.Reads==reads && mock.Acquired==acquired && mock.Closed==acquired && owner.TokensClosed,
                "access failure retry/leak or unexpected read count");
        }
        public static void CheckOracles() {
            Require(IntPtr.Size==8 && Marshal.SizeOf(typeof(GenericMapping))==16 &&
                Marshal.OffsetOf(typeof(GenericMapping),"Read").ToInt32()==0 && Marshal.OffsetOf(typeof(GenericMapping),"Write").ToInt32()==4 &&
                Marshal.OffsetOf(typeof(GenericMapping),"Execute").ToInt32()==8 && Marshal.OffsetOf(typeof(GenericMapping),"All").ToInt32()==12,
                "AccessCheck x64/ARM64 mapping ABI");
            Require(Marshal.SizeOf(typeof(AccessLuidAttributes))==12 && Marshal.OffsetOf(typeof(AccessLuidAttributes),"Attributes").ToInt32()==8 &&
                Marshal.SizeOf(typeof(PrivilegeSetOne))==20 && Marshal.OffsetOf(typeof(PrivilegeSetOne),"First").ToInt32()==8,
                "AccessCheck privilege-set ABI");
            foreach(bool allowed in new bool[] { true,false }) {
                var mock=new MockAccessCalls { AccessAllowed=allowed }; var owner=new ParentAccess(mock);
                try {
                    owner.MeasureHandles(new IntPtr(1),new IntPtr(2));
                    Require(mock.Reads==8 && mock.Checks==4 && owner.Objects[1].Candidate.ApiSucceeded &&
                        owner.Objects[1].Candidate.LastError==null && owner.Objects[1].Candidate.AccessStatus==allowed &&
                        owner.Objects[1].Candidate.GrantedAccess==(allowed?(uint?)0x20041:null),"API success/stale error/access denial conflated");
                } finally { owner.Close(); }
                owner.Close(); Require(mock.Closed==2 && owner.TokensClosed,"access close not idempotent");
            }
            Refuse(new MockAccessCalls { FailDuplicate=1 },5,0,0);
            Refuse(new MockAccessCalls { FailDuplicate=2 },5,0,1);
            Refuse(new MockAccessCalls { FailRead=1 },5,1,2);
            Refuse(new MockAccessCalls { FailRead=3 },5,3,2);
            Refuse(new MockAccessCalls { FailRead=4 },5,4,2);
            Refuse(new MockAccessCalls { ProbeError=24 },24,1,2);
            Refuse(new MockAccessCalls { ProbeSuccess=true },-1,1,2);
            Refuse(new MockAccessCalls { ReadSuccess=false },5,2,2);
            Refuse(new MockAccessCalls { NameProbeLength=0 },-1,1,2);
            Refuse(new MockAccessCalls { NameProbeLength=3 },-1,1,2);
            Refuse(new MockAccessCalls { NameProbeLength=514 },-1,1,2);
            Refuse(new MockAccessCalls { NameReturnLength=12 },-1,2,2);
            Refuse(new MockAccessCalls { ProbeLength=19 },-1,3,2);
            Refuse(new MockAccessCalls { ProbeLength=65537 },-1,3,2);
            Refuse(new MockAccessCalls { ReturnLength=39 },-1,4,2);
            Refuse(new MockAccessCalls { ReturnLength=41 },-1,4,2);
            Refuse(new MockAccessCalls { AccessSuccess=false },5,4,2);
            Refuse(new MockAccessCalls { PrivilegeLength=7 },-1,4,2);
            Refuse(new MockAccessCalls { PrivilegeLength=65537 },-1,4,2);
            Refuse(new MockAccessCalls { PrivilegeCount=1 },-1,4,2);
            Refuse(new MockAccessCalls { PrivilegeCount=257,PrivilegeLength=65536 },-1,4,2);
            Refuse(new MockAccessCalls { PrivilegeControl=2 },-1,4,2);
            Refuse(new MockAccessCalls { GrantedOverride=0x80000000 },-1,4,2);
            var privilegeMock=new MockAccessCalls { PrivilegeLength=20,PrivilegeCount=1 };
            var privilegeOwner=new ParentAccess(privilegeMock);
            try { privilegeOwner.MeasureHandles(new IntPtr(1),new IntPtr(2)); }
            finally { privilegeOwner.Close(); }
            Require(privilegeMock.Checks==4 && privilegeMock.Closed==2,"valid privilege-set stride refused or leaked");
            var capMock=new MockAccessCalls { ProbeLength=65536,ReturnLength=65536 };
            var capOwner=new ParentAccess(capMock);
            try { capOwner.MeasureHandles(new IntPtr(1),new IntPtr(2)); }
            finally { capOwner.Close(); }
            Require(capMock.Checks==4 && capMock.Closed==2,"maximum descriptor capacity refused or leaked");
            var broken=MockDescriptor(); Array.Copy(BitConverter.GetBytes(65532U),0,broken,16,4);
            bool rejected=false; try { DescriptorBounds(broken); } catch(InvalidOperationException) { rejected=true; }
            Require(rejected,"descriptor offset escaped bounds");
            rejected=false; try { ObjectName(Encoding.Unicode.GetBytes("bad\0name\0")); } catch(InvalidOperationException) { rejected=true; }
            Require(rejected,"embedded object name terminator accepted");
            Require(ObjectName(Encoding.Unicode.GetBytes(new string('x',255)+"\0")).Length==255,"maximum name capacity refused");
            var closeMock=new MockAccessCalls { FailClose=true }; var closeOwner=new ParentAccess(closeMock);
            closeOwner.MeasureHandles(new IntPtr(1),new IntPtr(2)); rejected=false;
            try { closeOwner.Close(); } catch(Win32Exception e) { rejected=e.NativeErrorCode==6; }
            Require(rejected && closeMock.Closed==1 && !closeOwner.TokensClosed && closeOwner.baselineCopy==new IntPtr(301) &&
                closeOwner.candidateCopy==IntPtr.Zero,"partial token close lost ownership or skipped other token");
            closeMock.FailClose=false; closeOwner.Close(); Require(closeMock.Closed==2 && closeOwner.TokensClosed,"retained mock ownership not closable");
        }
    }
}
'@

    [KwilTokenProof]::CheckLayout()
    if ($Mode -ceq 'Driver') { [KwilTokenProof]::CheckInfoBufferOracles() }
    $current = [KwilTokenProof]::Current($Mode -ceq 'Driver')
    $report.baseline = [KwilTokenProof]::Measure($current)
    if ($Mode -ceq 'Worker') {
        # No baseline or candidate claims supplied by the driver: query own OS
        # primary/effective context, then parent compares independent receipts.
        [KwilTokenProof]::Standard($report.baseline)
        Assert-TokenCase ($script:Clock.ElapsedMilliseconds -lt 15000) 'worker inspection budget exhausted'
        $proof = [ordered]@{schema=1;status='PASS';identity=$identity;nonce=$OwnerNonce
            pid=[KwilTokenProof]::GetCurrentProcessId();facts=$report.baseline}
        Write-BoundedJson 'worker-proof.json' $proof
        $report.status = 'PASS'
    } else {
        # Focused pure/mocked oracles are authored here, NOT claimed executed by
        # the source-only implementation job. They run only after hosted admission.
        $oracles = [Collections.Generic.List[string]]::new()
        $oracles.Add('fixed-ABI-one-call-and-bounded-variable-info-buffers')
        Assert-TokenCase ((Get-LaunchAdmission 32000) -eq 2000) 'late admission did not cap to 2s'
        $refused = $false
        try { $null = Get-LaunchAdmission 30000 } catch { $refused = $true }
        Assert-TokenCase $refused 'cleanup/evidence-only admission allowed creation'
        $oracles.Add('late-budget-caps-and-refuses')
        $mock = [pscustomobject]@{closed=$false;work=$false}
        $seen = $false
        try {
            $null = Invoke-OwnedProof { $mock } { param($o) throw [IO.IOException]::new('mock-receipt-failure') } { param($o) $o.work=$true } { param($o) $o.closed=$true; throw 'mock-cleanup-failure' }
        } catch { $seen = $_.Exception.Message -ceq 'mock-receipt-failure' -and $_.Exception.Data['cleanupFailure'] -ceq 'mock-cleanup-failure' }
        Assert-TokenCase ($seen -and $mock.closed -and -not $mock.work) 'receipt failure lost primary error or cleanup'
        $oracles.Add('receipt-failure-closes-and-preserves-primary')
        $refused = $false
        try { [KwilTokenProof]::OwnedIdentity(101,202) } catch { $refused=$true }
        Assert-TokenCase $refused 'foreign PID was admitted'
        $oracles.Add('foreign-pid-refused-before-handle-use')
        [KwilTokenProof+Lease]::CheckCleanupDeadlineOracle()
        $oracles.Add('late-watchdog-and-both-cleanups-share-deadline-retain-handles')
        $good = [KwilTokenProof+Facts]::new()
        $good.UserSid='S-1-5-21-1-2-3-1001'; $good.AuthenticationId='00000000:00001234'; $good.LogonSid='S-1-5-5-1-2'
        $good.SessionId=1; $good.TokenType=1; $good.TokenElevationType=1
        [KwilTokenProof]::Standard($good)
        foreach ($field in @('UserSid','AuthenticationId','LogonSid','SessionId','TokenElevation','TokenElevationType','EffectiveAdmin','AdminSidAttributes')) {
            $bad = [KwilTokenProof+Facts]::new()
            foreach ($name in @('UserSid','AuthenticationId','LogonSid','SessionId','TokenType','TokenElevationType')) { $bad.$name=$good.$name }
            switch ($field) {
                'UserSid' { $bad.UserSid='S-1-5-21-9-9-9-1001' }
                'AuthenticationId' { $bad.AuthenticationId='00000000:00009999' }
                'LogonSid' { $bad.LogonSid='S-1-5-5-9-9' }
                'SessionId' { $bad.SessionId=0 }
                'TokenElevation' { $bad.TokenElevation=1 } # Default still MUST fail.
                'TokenElevationType' { $bad.TokenElevationType=2 }
                'EffectiveAdmin' { $bad.EffectiveAdmin=$true }
                'AdminSidAttributes' { $bad.AdminSidPresent=$true; $bad.AdminSidAttributes=4 }
            }
            $refused=$false
            try { [KwilTokenProof]::SameIdentity($good,$bad); [KwilTokenProof]::Standard($bad) } catch { $refused=$true }
            Assert-TokenCase $refused "mock policy accepted changed $field"
            $oracles.Add("refuses-$field")
        }
        # Invoke Standard directly: identity mismatch must not short-circuit
        # these policy negatives (especially the previously masked session 0).
        foreach ($case in @(
            @{name='session-zero';field='SessionId';value=0},
            @{name='local-system';field='UserSid';value='S-1-5-18'},
            @{name='local-service';field='UserSid';value='S-1-5-19'},
            @{name='network-service';field='UserSid';value='S-1-5-20'},
            @{name='service-sid';field='UserSid';value='S-1-5-80-1-2-3-4-5'},
            @{name='impersonation-token';field='TokenType';value=2},
            @{name='invalid-token-type';field='TokenType';value=0}
        )) {
            $bad = [KwilTokenProof+Facts]::new()
            foreach ($name in @('UserSid','AuthenticationId','LogonSid','SessionId','TokenType','TokenElevationType')) { $bad.$name=$good.$name }
            $bad.($case.field)=$case.value
            $refused=$false
            try { [KwilTokenProof]::Standard($bad) } catch { $refused=$true }
            Assert-TokenCase $refused "Standard directly accepted $($case.name)"
            $oracles.Add("standard-refuses-$($case.name)")
        }
        $report.oracles = $oracles.ToArray()
        $report.selection = [KwilTokenProof+Selection]::new()
        $candidate = [KwilTokenProof]::Select($current, $report.baseline, $report.selection)
        $nonce = [Guid]::NewGuid().ToString('N') # correlation, not a credential
        $pwsh = Join-Path $PSHOME 'pwsh.exe'
        Assert-TokenCase ([IO.Path]::IsPathFullyQualified($pwsh) -and (Test-Path -LiteralPath $pwsh -PathType Leaf)) 'absolute existing pwsh required'
        Assert-PlainPath $pwsh
        # Reject quotes/controls before simple Windows argv quoting (these args
        # are files or fixed enums/nonces, none ends in a backslash).
        foreach ($arg in @($pwsh,$PSCommandPath,$EvidenceDirectory)) {
            Assert-TokenCase ($arg -notmatch '["\x00-\x1f]' -and -not $arg.EndsWith('\')) 'unsafe command argument'
        }
        $command = '"{0}" -NoLogo -NoProfile -NonInteractive -File "{1}" -Mode Worker -Architecture {2} -EvidenceDirectory "{3}" -OwnerNonce {4}' -f $pwsh,$PSCommandPath,$Architecture,$EvidenceDirectory,$nonce
        $application = $pwsh
        if ($Purpose -ceq 'NativeCommandStartup') {
            [KwilTokenProof+ParentAccess]::CheckOracles()
            $oracles.Add('parent-access-bounded-reads-mappings-errors-and-exact-partial-ownership')
            $report.oracles = $oracles.ToArray()
            Assert-TokenCase ($env:SystemRoot -and [IO.Path]::IsPathFullyQualified($env:SystemRoot)) 'absolute SystemRoot required for native command'
            $application = Join-Path $env:SystemRoot 'System32/cmd.exe'
            Assert-TokenCase ([IO.Path]::IsPathFullyQualified($application) -and (Test-Path -LiteralPath $application -PathType Leaf)) 'absolute existing native system command required'
            Assert-PlainPath $application
            Assert-TokenCase ($application -notmatch '["\x00-\x1f]' -and -not $application.EndsWith('\')) 'unsafe native command path'
            $report.nativeCommand.machine = Assert-NativeCommandImage $application
            $command = '"{0}" /d /q /c "exit /b 73"' -f $application
            $script:ParentAccess = [KwilTokenProof+ParentAccess]::new()
            $report.parentAccess = $script:ParentAccess
            # Reuse the existing finally/primary-error seam; the owner is public
            # facts plus private duplicate handles, not a second process lease.
            $null = Invoke-OwnedProof { $script:ParentAccess } { param($owner) } {
                param($owner)
                $owner.Measure($current,$candidate)
            } {
                param($owner)
                $owner.Close()
            }
            Write-BoundedJson 'parent-access.json' ([ordered]@{schema=2;purpose=$Purpose;identity=$identity
                observations=$report.parentAccess;interpretation='parent-DACL-only; not child attachment, MIC, DLL attribution or worker qualification'})
        }
        $environment = [ordered]@{
            GITHUB_ACTIONS='true';RUNNER_ENVIRONMENT='github-hosted';RUNNER_OS='Windows';RUNNER_ARCH=$env:RUNNER_ARCH
            GITHUB_REPOSITORY=$env:GITHUB_REPOSITORY;GITHUB_RUN_ID=$env:GITHUB_RUN_ID;GITHUB_RUN_ATTEMPT=$env:GITHUB_RUN_ATTEMPT;GITHUB_SHA=$env:GITHUB_SHA
            GITHUB_WORKSPACE=$Repository;KWIL_INSTALLER_TOKEN_DISPOSABLE=$env:KWIL_INSTALLER_TOKEN_DISPOSABLE
            KWIL_TOKEN_WORKER_NONCE=$nonce;SystemRoot=$env:SystemRoot;WINDIR=$env:WINDIR
            TEMP=$EvidenceDirectory;TMP=$EvidenceDirectory;POWERSHELL_TELEMETRY_OPTOUT='1';POWERSHELL_UPDATECHECK='Off'
            DOTNET_CLI_HOME=$EvidenceDirectory;DOTNET_ADD_GLOBAL_TOOLS_TO_PATH='0';DOTNET_SKIP_FIRST_TIME_EXPERIENCE='1';DOTNET_NOLOGO='1'
        }
        foreach ($entry in $environment.GetEnumerator()) { Assert-TokenCase ($entry.Value -and [string]$entry.Value -notmatch "[\x00\r\n]") 'invalid private environment entry' }
        $block = (@($environment.Keys | Sort-Object | ForEach-Object { "$_=$($environment[$_])" }) -join [char]0) + [char]0 + [char]0
        if ($Purpose -ceq 'NativeCommandStartup') {
            $report.nativeCommand.environmentNames = @($environment.Keys | Sort-Object)
            $report.nativeCommand.environmentUtf16Bytes = [Text.Encoding]::Unicode.GetByteCount($block)
        }
        # Recheck admission AFTER candidate/env/path work, immediately before API.
        $proof = Invoke-OwnedProof {
            $lifetime = Get-LaunchAdmission ($script:BudgetMs - $script:Clock.ElapsedMilliseconds)
            $script:Lease = [KwilTokenProof]::Launch($candidate,$application,$command,$block,$EvidenceDirectory,$lifetime,[int]$script:CleanupReserveMs)
            $script:Lease
        } {
            param($lease)
            $ownedReceipt = [ordered]@{identity=$identity;nonce=$nonce;driverPid=$PID
                childPid=$lease.Pid;childThreadId=$lease.ThreadId;lifetimeMilliseconds=$lease.LifetimeMilliseconds
                suspended=$true;watchdogArmedBeforeReceipt=$true;baseline=$report.baseline;selection=$report.selection}
            if ($Purpose -ceq 'NativeCommandStartup') { $ownedReceipt['schema']=2; $ownedReceipt['purpose']=$Purpose }
            Write-BoundedJson 'owned-process.json' $ownedReceipt
        } {
            param($lease)
            $report.suspended = $lease.InspectSuspended()
            [KwilTokenProof]::SameIdentity($report.baseline,$report.suspended)
            [KwilTokenProof]::Standard($report.suspended)
            Assert-TokenFactsAgree $report.selection.CandidateFacts $report.suspended 'candidate/suspended'
            $lease.Resume()
            if ($Purpose -ceq 'NativeCommandStartup') {
                $report.nativeCommand.exitCode = $lease.Wait()
                $report.nativeCommand.reached = $report.nativeCommand.exitCode -eq 73
                Assert-TokenCase $report.nativeCommand.reached 'native command sentinel not reached; no second launch or fallback'
                return # No worker-proof read and no worker acceptance path.
            }
            Assert-TokenCase ($lease.Wait() -eq 0) 'worker returned failure; no elevated/credential retry'
            $path = Join-Path $EvidenceDirectory 'worker-proof.json'
            $actual = Read-BoundedProof $path
            Assert-TokenCase ($actual.schema -eq 1 -and $actual.status -ceq 'PASS' -and $actual.nonce -ceq $nonce) 'worker proof correlation/status mismatch'
            Assert-TokenCase ($actual.pid -eq $lease.Pid) 'worker proof PID mismatch'
            foreach ($key in $identity.Keys) { Assert-TokenCase ($actual.identity.$key -ceq $identity[$key]) "worker source identity mismatch: $key" }
            # Compare every public token fact independently measured in the
            # suspended child to its own actual-worker measurement, not an argv assertion.
            Assert-TokenFactsAgree $report.suspended $actual.facts 'suspended/actual'
            $actual
        } {
            param($lease)
            $script:Cleanup = $lease.Close()
            Assert-TokenCase $script:Cleanup.Success 'exact owned process/handles not cleanly reaped/closed'
            Assert-TokenCase (-not $script:Cleanup.DeadlineExpired) 'worker watchdog deadline expired'
        }
        if ($Purpose -ceq 'NativeCommandStartup') {
            Assert-TokenCase (-not (Test-Path -LiteralPath (Join-Path $EvidenceDirectory 'worker-proof.json'))) 'unexpected worker proof in native-only diagnostic'
            Assert-TokenCase (-not (Test-Path -LiteralPath (Join-Path $EvidenceDirectory 'worker-result.json'))) 'unexpected worker result in native-only diagnostic'
            $report.status = 'DIAGNOSTIC-COMPLETE'
        } else {
            $report.actual = $proof
            $report.status = 'PASS'
        }
    }
} catch {
    $primaryError = $_
    # Recover only the original native ownership object, never reopen by PID.
    # Its launch-failure reap and every final reap share its existing deadline.
    $exception = $_.Exception
    for ($depth=0; $null -ne $exception -and $depth -lt 8; $depth++) {
        if ($null -eq $script:Lease -and $exception.Data.Contains('launchLease')) { $script:Lease=$exception.Data['launchLease'] }
        $exception = $exception.InnerException
    }
    $report.error = Get-PublicFailure $_
}
finally {
    # Only this invocation's exact object can close/terminate this process.
    # Cleanup is complete BEFORE final receipt writes; timer survives earlier IO.
    $cleanupErrors = [Collections.Generic.List[string]]::new()
    if ($script:Lease) {
        try { $script:Cleanup=$script:Lease.Close(); $report.cleanup=$script:Cleanup
            if (-not $script:Cleanup.Success) { $cleanupErrors.Add('unresolved exact child ownership') }
        } catch { $cleanupErrors.Add($_.Exception.Message) }
    }
    if ($script:ParentAccess) {
        try { $script:ParentAccess.Close() } catch { $cleanupErrors.Add($_.Exception.Message) }
    }
    if ($Purpose -ceq 'NativeCommandStartup') {
        foreach ($artifact in @(@{field='workerProof';name='worker-proof.json'},@{field='workerResult';name='worker-result.json'})) {
            try {
                $present = Test-Path -LiteralPath (Join-Path $EvidenceDirectory $artifact.name)
                $report.($artifact.field) = if ($present) { 'unexpected-present' } else { 'absent' }
                if ($present) { $cleanupErrors.Add('unexpected worker artifact in native diagnostic') }
            } catch { $cleanupErrors.Add($_.Exception.Message) }
        }
    }
    foreach ($token in @($candidate,$current)) {
        if ($null -ne $token) { try { $token.Dispose() } catch { $cleanupErrors.Add($_.Exception.Message) } }
    }
    if ($script:Clock.ElapsedMilliseconds -ge $script:BudgetMs) { $cleanupErrors.Add('internal 90s deadline exhausted') }
    if ($cleanupErrors.Count -gt 0) {
        $report.status='FAIL/design-fork'
        if ($primaryError) { $primaryError.Exception.Data['finalCleanup']=$cleanupErrors -join '; ' }
        else { $primaryError=[Management.Automation.ErrorRecord]::new([InvalidOperationException]::new(($cleanupErrors -join '; ')),'TokenProofCleanup',[Management.Automation.ErrorCategory]::OperationStopped,$null); $report.error=Get-PublicFailure $primaryError }
    }
    $report['elapsedBeforeFinalEvidenceMilliseconds']=$script:Clock.ElapsedMilliseconds
    $report['cleanupDiagnostics']=$cleanupErrors.ToArray()
    if ($primaryError) { $report.error=Get-PublicFailure $primaryError }
    try {
        Write-BoundedJson "$($Mode.ToLowerInvariant())-result.json" $report
        if ($Mode -ceq 'Driver') {
            if ($Purpose -ceq 'NativeCommandStartup') { Write-NativeEvidenceManifest $identity $report.status }
            else { Write-EvidenceManifest $identity $report.status }
        }
    }
    catch {
        if ($primaryError) { $primaryError.Exception.Data['finalReceiptFailure']=$_.Exception.Message }
        else { $primaryError=$_ }
    }
}
if ($primaryError) {
    $failure = Get-PublicFailure $primaryError
    [Console]::Error.WriteLine("commit=$($identity.commit) mode=$Mode status=FAIL/design-fork $($failure.message)")
    foreach ($diagnostic in $failure.diagnostics) { [Console]::Error.WriteLine($diagnostic) }
    throw $primaryError
}
if ($Purpose -ceq 'NativeCommandStartup') {
    [Console]::WriteLine("commit=$($identity.commit) purpose=NativeCommandStartup diagnostic=COMPLETE native-command-reached=true token-preflight=NOT-RUN worker-proof=ABSENT installer-qualification=NOT-RUN")
} else {
    [Console]::WriteLine("commit=$($identity.commit) mode=$Mode token-preflight=PASS installer-qualification=NOT-RUN")
}
