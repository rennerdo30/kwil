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
Only the bounded public facts enumerated in Facts are serialized.

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
    [string]$OwnerNonce = ''
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$script:Clock = [Diagnostics.Stopwatch]::StartNew()
$script:BudgetMs = 90000L
$script:CleanupReserveMs = 10000L
$script:EvidenceReserveMs = 20000L
$script:Lease = $null
$script:Cleanup = $null

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

    static Exception Error(string api) { int e=Marshal.GetLastWin32Error();
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
    sealed class Info : IDisposable {
        internal IntPtr Data; internal uint Length;
        internal Info(IntPtr token,int kind) {
            uint n; bool ok=GetTokenInformation(token,kind,IntPtr.Zero,0,out n); int error=Marshal.GetLastWin32Error();
            Require(!ok && error==122 && n>0 && n<=MaxTokenBytes,"GetTokenInformation size class="+kind+" error="+error);
            Data=Marshal.AllocHGlobal(checked((int)n));
            try { if(!GetTokenInformation(token,kind,Data,n,out Length)) throw Error("GetTokenInformation class="+kind);
                Require(Length>0 && Length<=n,"token information size changed"); }
            catch { Dispose(); throw; }
        }
        internal void AtLeast(int bytes) { Require(Length>=bytes,"short token information"); }
        public void Dispose() { if(Data!=IntPtr.Zero) { Marshal.FreeHGlobal(Data); Data=IntPtr.Zero; } }
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
}
'@

    [KwilTokenProof]::CheckLayout()
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
        # Recheck admission AFTER candidate/env/path work, immediately before API.
        $proof = Invoke-OwnedProof {
            $lifetime = Get-LaunchAdmission ($script:BudgetMs - $script:Clock.ElapsedMilliseconds)
            $script:Lease = [KwilTokenProof]::Launch($candidate,$pwsh,$command,$block,$EvidenceDirectory,$lifetime,[int]$script:CleanupReserveMs)
            $script:Lease
        } {
            param($lease)
            Write-BoundedJson 'owned-process.json' ([ordered]@{identity=$identity;nonce=$nonce;driverPid=$PID
                childPid=$lease.Pid;childThreadId=$lease.ThreadId;lifetimeMilliseconds=$lease.LifetimeMilliseconds
                suspended=$true;watchdogArmedBeforeReceipt=$true;baseline=$report.baseline;selection=$report.selection})
        } {
            param($lease)
            $report.suspended = $lease.InspectSuspended()
            [KwilTokenProof]::SameIdentity($report.baseline,$report.suspended)
            [KwilTokenProof]::Standard($report.suspended)
            Assert-TokenFactsAgree $report.selection.CandidateFacts $report.suspended 'candidate/suspended'
            $lease.Resume()
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
        $report.actual = $proof
        $report.status = 'PASS'
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
        if ($Mode -ceq 'Driver') { Write-EvidenceManifest $identity $report.status }
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
[Console]::WriteLine("commit=$($identity.commit) mode=$Mode token-preflight=PASS installer-qualification=NOT-RUN")
