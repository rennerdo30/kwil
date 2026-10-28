#requires -Version 7.4
<#
Trusted disposable-host fixture only. SOURCE candidate: functional host facts and
candidate desktop rights are UNVERIFIED. Manual dispatch is not owner approval.
S/H are the trusted A boundary; C/W are the fresh ordinary B boundary.
No MSI, production token-policy fallback, account/profile deletion or retry.
#>
[CmdletBinding()]
param(
    [ValidateSet('Supervisor','Helper')][string]$Role='Supervisor',
    [string]$SpecPath='',
    [string]$PipeName='',
    [switch]$LibraryOnly
)
Set-StrictMode -Version Latest
$ErrorActionPreference='Stop'
function Assert-OU([bool]$Condition,[string]$Code) {
    if(-not $Condition) { throw "ordinary-user:$Code" }
}
function Get-OUHash([string]$Path) {
    $s=[IO.File]::OpenRead($Path)
    try { Assert-OU ($s.Length -le 1048576) 'source-size'; [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($s)).ToLowerInvariant() }
    finally { $s.Dispose() }
}
function Get-OUTextHash([string]$Text) {
    [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($Text))).ToLowerInvariant()
}
function Get-OURemaining([long]$Deadline,[int]$Reserve=0) {
    $left=$Deadline-[Environment]::TickCount64-$Reserve
    Assert-OU ($left -gt 0) 'absolute-deadline'
    [int][Math]::Min([int]::MaxValue,$left)
}
# Strict wire/spec readers: bounded UTF-8, exact fields, no duplicate decoded keys.
# Only schema is a numeric member; no secret is ever permitted in these objects.
function Assert-OUJsonElement([System.Text.Json.JsonElement]$Element,[int]$Depth=0) {
    Assert-OU ($Depth -le 4) 'json-depth'
    switch($Element.ValueKind) {
        Object {
            $keys=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            foreach($p in $Element.EnumerateObject()) {
                Assert-OU ($keys.Add($p.Name)) 'json-duplicate'
                if($p.Name -ceq 'schema') {
                    Assert-OU ($p.Value.ValueKind -eq [Text.Json.JsonValueKind]::Number -and $p.Value.GetRawText() -ceq '1') 'json-schema'
                } else { Assert-OU ($p.Value.ValueKind -eq [Text.Json.JsonValueKind]::String) 'json-member-kind' }
                Assert-OUJsonElement $p.Value ($Depth+1)
            }
        }
        String {
            $t=$Element.GetString()
            Assert-OU ($null -ne $t -and $t.Length -le 4096 -and $t.IndexOfAny([char[]]@([char]0,[char]10,[char]13)) -lt 0) 'json-string'
        }
        Number { Assert-OU ($Element.GetRawText() -ceq '1') 'json-number' }
        default { throw 'ordinary-user:json-kind' }
    }
}
function ConvertFrom-OUObject([string]$Text,[string[]]$Fields) {
    Assert-OU ([Text.Encoding]::UTF8.GetByteCount($Text) -le 32768) 'json-bound'
    $d=[Text.Json.JsonDocument]::Parse($Text,[Text.Json.JsonDocumentOptions]@{MaxDepth=4})
    try { Assert-OU ($d.RootElement.ValueKind -eq [Text.Json.JsonValueKind]::Object) 'json-object'; Assert-OUJsonElement $d.RootElement }
    finally { $d.Dispose() }
    $v=ConvertFrom-Json -InputObject $Text -AsHashtable -Depth 4
    Assert-OU ($v.Count -eq $Fields.Count) 'json-fields'
    foreach($f in $Fields) { Assert-OU ($v.Contains($f)) 'json-required-field' }
    return $v
}
function Read-OUObject([string]$Path,[string[]]$Fields) {
    $s=[IO.File]::OpenRead($Path)
    try { Assert-OU ($s.Length -gt 0 -and $s.Length -le 32768) 'json-file-size'; $b=[byte[]]::new([int]$s.Length); $s.ReadExactly($b) }
    finally { $s.Dispose() }
    $text=[Text.UTF8Encoding]::new($false,$true).GetString($b)
    Assert-OU (-not $text.StartsWith([string][char]0xfeff)) 'json-bom'
    ConvertFrom-OUObject $text $Fields
}
function Read-OUSpec([string]$Path) {
    $fields=@('schema','purpose','base','commit','run','attempt','architecture','nonce','root','machine','supervisorPid','supervisorCreated','supervisorDeadline','bootstrapHash','sessionHash','workflowHash','approvalHash','user','sid')
    $v=Read-OUObject $Path $fields
    Assert-OU ($v.schema -is [long] -and $v.schema -eq 1 -and $v.purpose -ceq 'OrdinaryUserSessionProof') 'spec-purpose'
    Assert-OU ($v.base -ceq '26da6914ace64b436ec868f29190cabb541a7b83') 'spec-base'
    Assert-OU ($v.commit -cmatch '\A[0-9a-f]{40}\z' -and $v.run -cmatch '\A[1-9][0-9]{0,19}\z' -and $v.attempt -ceq '1') 'spec-run'
    Assert-OU ($v.architecture -cin @('x64','arm64') -and $v.nonce -cmatch '\A[0-9a-f]{32}\z') 'spec-architecture-nonce'
    Assert-OU ($v.user -ceq ('kwo'+$v.nonce.Substring(0,12))) 'spec-account'
    foreach($f in @('bootstrapHash','sessionHash','workflowHash','approvalHash')) { Assert-OU ($v[$f] -cmatch '\A[0-9a-f]{64}\z') 'spec-hash' }
    foreach($f in @('supervisorPid','supervisorCreated','supervisorDeadline')) { Assert-OU ($v[$f] -cmatch '\A[1-9][0-9]{0,18}\z') 'spec-integer' }
    Assert-OU ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($Path)) -ceq $v.root) 'spec-root'
    return $v
}
function Write-OUJson([string]$Path,$Value) {
    $b=[Text.UTF8Encoding]::new($false,$true).GetBytes(($Value | ConvertTo-Json -Depth 8 -Compress)+[Environment]::NewLine)
    Assert-OU ($b.Length -le 32768) 'receipt-bound'
    $s=[IO.FileStream]::new($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
    try { $s.Write($b); $s.Flush($true) } finally { $s.Dispose() }
}
function Send-OUFrame($Wire,[string]$Kind,$Spec,[Collections.IDictionary]$Values,[long]$Deadline) {
    $frame=[ordered]@{kind=$Kind;nonce=$Spec.nonce}
    foreach($key in $Values.Keys) { Assert-OU ($Values[$key] -is [string]) 'frame-type'; $frame.Add($key,$Values[$key]) }
    $Wire.Write(($frame | ConvertTo-Json -Depth 3 -Compress),(Get-OURemaining $Deadline))
}
function Read-OUFrame($Wire,[string]$Kind,$Spec,[string[]]$Fields,[long]$Deadline) {
    $v=ConvertFrom-OUObject ($Wire.Read((Get-OURemaining $Deadline))) (@('kind','nonce')+$Fields)
    Assert-OU ($v.kind -ceq $Kind -and $v.nonce -ceq $Spec.nonce) 'frame-correlation'
    return $v
}
function Test-OUCanRestore([bool]$Ambiguous,[bool]$HelperStopped,[bool]$ControllerStopped,[bool]$JobEmpty,[bool]$PlanReady) {
    return (-not $Ambiguous -and $HelperStopped -and $ControllerStopped -and $JobEmpty -and $PlanReady)
}
function Assert-OUHost([string]$Architecture) {
    Assert-OU ($IsWindows -and $env:GITHUB_ACTIONS -ceq 'true' -and $env:RUNNER_ENVIRONMENT -ceq 'github-hosted') 'host-disposable'
    Assert-OU ($env:GITHUB_ACTOR -ceq 'rennerdo30' -and $env:GITHUB_EVENT_NAME -ceq 'workflow_dispatch' -and $env:GITHUB_REPOSITORY -ceq 'rennerdo30/kwil' -and $env:GITHUB_REF -ceq 'refs/heads/main') 'host-manual-main'
    Assert-OU ($env:GITHUB_RUN_ATTEMPT -ceq '1' -and $env:GITHUB_SHA -cmatch '\A[0-9a-f]{40}\z' -and $env:GITHUB_WORKFLOW_SHA -ceq $env:GITHUB_SHA) 'host-attempt-source'
    Assert-OU ($env:GITHUB_WORKFLOW_REF -ceq 'rennerdo30/kwil/.github/workflows/ordinary-user-proof.yml@refs/heads/main') 'host-workflow'
    $actual=[Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString().ToLowerInvariant()
    $os=[Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
    Assert-OU ($Architecture -cin @('x64','arm64') -and $actual -ceq $Architecture -and $os -ceq $Architecture) 'host-native-architecture'
    Assert-OU ($env:KWIL_DISPOSABLE_PROOF -ceq ($env:GITHUB_RUN_ID+':1:'+$env:GITHUB_SHA)) 'host-marker'
    Assert-OU ($env:KWIL_OWNER_APPROVAL_REFERENCE -cmatch '\A[a-zA-Z0-9][a-zA-Z0-9._:/-]{0,127}\z') 'owner-reference-required-not-authority'
}
function Assert-OUSource([string]$Bootstrap) {
    $repo=[IO.Path]::GetDirectoryName([IO.Path]::GetDirectoryName($Bootstrap))
    Assert-OU ([IO.Path]::GetFullPath($repo) -ceq [IO.Path]::GetFullPath($env:GITHUB_WORKSPACE)) 'source-workspace'
    Assert-OU ($env:KWIL_BOOTSTRAP_SHA256 -cmatch '\A[0-9a-f]{64}\z' -and $env:KWIL_SESSION_SHA256 -cmatch '\A[0-9a-f]{64}\z') 'source-pins-required'
    Assert-OU ((Get-OUHash $Bootstrap) -ceq $env:KWIL_BOOTSTRAP_SHA256 -and (Get-OUHash (Join-Path $repo 'scripts/ci-ordinary-user-session.ps1')) -ceq $env:KWIL_SESSION_SHA256) 'source-pin-mismatch'
    return $repo
}
function Set-OUFreshDirectoryAcl([string]$Path,[string]$OwnerSid,[string]$BSid='',[int]$BMask=0) {
    # Only an atomically created new run root/source/data/temp, never runner roots.
    $acl=[Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true,$false)
    $inherit=[Security.AccessControl.InheritanceFlags]'ContainerInherit,ObjectInherit'
    foreach($sid in @($OwnerSid,'S-1-5-18')) {
        $rule=[Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid),[Security.AccessControl.FileSystemRights]0x1f01ff,$inherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow)
        $acl.AddAccessRule($rule)
    }
    if($BSid) {
        Assert-OU ($BMask -in @(0x1200a9,0x1301bf)) 'fresh-file-grant-mask'
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($BSid),[Security.AccessControl.FileSystemRights]$BMask,$inherit,[Security.AccessControl.PropagationFlags]::None,[Security.AccessControl.AccessControlType]::Allow))
    }
    [IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($Path),$acl)
}
function Initialize-OUNative {
    if(-not ('KwilOrdinary' -as [type])) { Add-Type -TypeDefinition (Get-OUNativeSource) -CompilerOptions '/unsafe' -ErrorAction Stop }
    [KwilOrdinary]::Layout()
}
function Assert-OUProfileFacts($Facts) {
    [KwilOrdinary]::Ordinary($Facts)
    Assert-OU ($Facts.HkcuLoaded -and $Facts.Station -ceq 'WinSta0' -and $Facts.Desktop -ceq 'Default') 'actual-profile-desktop'
}
function Assert-OUFactsJson($Native,$Observed,[switch]$Profile) {
    foreach($field in @('UserSid','AuthenticationId','LogonSid','IntegritySid','SessionId','TokenType','TokenElevation','TokenElevationType','AdminSidPresent','EffectiveAdmin','PowerfulPrivilegePresent','Profile','Local','Programs')) {
        $expected=$Native.$field; $actual=$Observed.$field
        if($expected -is [bool]) { Assert-OU ($actual -is [bool]) 'fact-boolean-kind' }
        elseif($expected -is [string]) { Assert-OU ($actual -is [string]) 'fact-string-kind' }
        else { Assert-OU ($actual -is [long] -and $actual -ge 0) 'fact-integer-kind' }
        Assert-OU ($expected -ceq $actual) ('fact-'+$field)
    }
    if($Profile) { Assert-OU ($Observed.HkcuLoaded -is [bool] -and $Observed.HkcuLoaded -eq $true -and $Observed.Station -ceq 'WinSta0' -and $Observed.Desktop -ceq 'Default') 'observed-profile-desktop' }
}
function New-OUCommand([string]$Application,[string]$Script,[string]$Tail) {
    foreach($s in @($Application,$Script)) { Assert-OU ($s -notmatch '["\x00\r\n]' -and -not $s.EndsWith('\')) 'command-fixed-path' }
    '"'+$Application+'" -NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -File "'+$Script+'" '+$Tail
}
function Read-OUProof([string]$Path,$Spec,[string]$Kind) {
    $s=[IO.File]::OpenRead($Path)
    try { Assert-OU ($s.Length -gt 0 -and $s.Length -le 32768) 'proof-size'; $b=[byte[]]::new([int]$s.Length); $s.ReadExactly($b) }
    finally { $s.Dispose() }
    $text=[Text.UTF8Encoding]::new($false,$true).GetString($b)
    $doc=[Text.Json.JsonDocument]::Parse($text,[Text.Json.JsonDocumentOptions]@{MaxDepth=8})
    try { Assert-OUProofJson $doc.RootElement } finally { $doc.Dispose() }
    $v=$text | ConvertFrom-Json -Depth 8
    Assert-OU ($v.schema -is [long] -and $v.schema -eq 1 -and $v.pid -is [long] -and $v.pid -gt 0 -and $v.created -is [long] -and $v.created -gt 0) 'proof-integer-kinds'
    Assert-OU ($v.purpose -ceq 'OrdinaryUserSessionProof' -and $v.base -ceq $Spec.base) 'proof-purpose-base'
    Assert-OU ($v.fullLifecycleAccepted -is [bool] -and -not $v.fullLifecycleAccepted -and $v.installerQualification -ceq 'not-run') 'proof-nonqualification'
    Assert-OU ($v.kind -ceq $Kind -and $v.status -ceq 'ready-natural-exit' -and $v.nonce -ceq $Spec.nonce -and $v.commit -ceq $Spec.commit -and $v.run -ceq $Spec.run -and $v.attempt -ceq '1' -and $v.architecture -ceq $Spec.architecture) 'proof-correlation'
    Assert-OU ($v.bootstrapHash -ceq $Spec.bootstrapHash -and $v.sessionHash -ceq $Spec.sessionHash -and $v.workflowHash -ceq $Spec.workflowHash) 'proof-source'
    return $v
}

function Assert-OUActualReceipts($Controller,$Worker,$Transfer,[uint32]$SupervisorPid) {
    Assert-OU ($Controller.pid -eq [uint32]$Transfer.pid -and $Controller.created -eq [long]$Transfer.created -and
        $Controller.supervisorPid -eq $SupervisorPid -and $Controller.workerPid -eq $Worker.pid -and
        $Controller.workerCreated -eq $Worker.created -and $Worker.controllerPid -eq $Controller.pid -and
        $Worker.controllerCreated -eq $Controller.created) 'actual-owned-pids-and-creation'
}
function Assert-OUProofJson([Text.Json.JsonElement]$Element,[int]$Depth=0) {
    Assert-OU ($Depth -le 6) 'proof-depth'
    switch($Element.ValueKind) {
        Object {
            $names=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            foreach($member in $Element.EnumerateObject()) { Assert-OU ($names.Add($member.Name)) 'proof-duplicate'; Assert-OUProofJson $member.Value ($Depth+1) }
        }
        String {
            $str=$Element.GetString()
            Assert-OU ($str.Length -le 4096 -and $str.IndexOfAny([char[]]@([char]0,[char]10,[char]13)) -lt 0) 'proof-string'
        }
        Number { Assert-OU ($Element.GetRawText() -cmatch '\A(?:0|[1-9][0-9]{0,18})\z') 'proof-integer' }
        True {}
        False {}
        default { throw 'ordinary-user:proof-kind' }
    }
}

function Test-OUEnvironmentSuccess([bool]$SourceProof,[string]$Failure,[bool]$Restored,[bool]$JobEmpty,[bool]$Ambiguous,[bool]$HelperComplete) {
    return ($SourceProof -and $Failure -ceq '' -and $Restored -and $JobEmpty -and -not $Ambiguous -and $HelperComplete)
}
function Get-OUTokenSummary($Facts) {
    # Fixed scalar facts only; no raw token/privilege/descriptor/environment dump.
    [ordered]@{sid=$Facts.UserSid;authentication=$Facts.AuthenticationId;logon=$Facts.LogonSid;session=$Facts.SessionId;type=$Facts.TokenType;integrity=$Facts.IntegritySid;elevation=$Facts.TokenElevation;elevationType=$Facts.TokenElevationType;adminSidPresent=$Facts.AdminSidPresent;effectiveAdmin=$Facts.EffectiveAdmin;powerfulPrivilegePresent=$Facts.PowerfulPrivilegePresent}
}

function Assert-OUTrustedCallerFacts($Facts) {
    Assert-OU ($Facts.EffectiveAdmin -and $Facts.SessionId -ne 0 -and $Facts.UserSid -notin @('S-1-5-18','S-1-5-19','S-1-5-20') -and $Facts.Station -ceq 'WinSta0' -and $Facts.Desktop -ceq 'Default') 'trusted-non-system-session'
    Assert-OU ($Facts.HkcuLoaded -and [IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\') -ceq $Facts.Profile -and [IO.Path]::GetFullPath($env:LOCALAPPDATA).TrimEnd('\') -ceq $Facts.Local) 'preexisting-normal-caller-profile'
}
function Get-OUNativeSource {
    return @'
using System;
using System.IO;
using System.IO.Pipes;
using System.Linq;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using System.Diagnostics;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Security.Principal;
using Microsoft.Win32;

// Fixed fixture ABI. No production guard/diagnostic dependency or token mutation.
public static class KwilOrdinary {
    public const uint StationGrant=0x22, DesktopGrant=0x83;
    public const uint StationOpen=0x60002, DesktopOpen=0x60081;
    public const uint CreationFlags=0x08000404; // suspended | Unicode env | no window
    public const int MaxBytes=65536, MaxGroups=256, MaxAces=256;
    public static void Need(bool ok,string code) { if(!ok) throw new InvalidOperationException("ordinary-user:"+code); }
    static Exception Error(string code) { return new Win32Exception(Marshal.GetLastWin32Error(),"ordinary-user:"+code); }
    [StructLayout(LayoutKind.Sequential)] public struct SI {
        public uint cb; public IntPtr reserved,desktop,title;
        public uint x,y,width,height,xChars,yChars,fill,flags; public ushort show,reservedBytes;
        public IntPtr reserved2,input,output,error;
    }
    [StructLayout(LayoutKind.Sequential)] public struct PI { public IntPtr process,thread; public uint pid,tid; }
    [StructLayout(LayoutKind.Sequential)] struct Luid {
        public uint low; public int high;
        public override string ToString() { return unchecked((uint)high).ToString("x8")+":"+low.ToString("x8"); }
    }
    [StructLayout(LayoutKind.Sequential)] struct Stats {
        public Luid token,authentication; public long expiration; public int type,impersonation;
        public uint charged,available,groups,privileges; public Luid modified;
    }
    [StructLayout(LayoutKind.Sequential)] struct SidAttr { public IntPtr sid; public uint attributes; }
    [StructLayout(LayoutKind.Sequential)] struct GroupsOne { public uint count; public SidAttr first; }
    [StructLayout(LayoutKind.Sequential)] struct User1 {
        public IntPtr name,password; public uint age,privilege; public IntPtr home,comment; public uint flags; public IntPtr script;
    }
    [StructLayout(LayoutKind.Sequential)] struct User23 { public IntPtr name,fullName,comment; public uint flags; public IntPtr sid; }
    [StructLayout(LayoutKind.Sequential)] struct BasicLimit {
        public long processTime,jobTime; public uint flags; public UIntPtr minWS,maxWS; public uint active;
        public UIntPtr affinity; public uint priority,scheduling;
    }
    [StructLayout(LayoutKind.Sequential)] struct IO { public ulong a,b,c,d,e,f; }
    [StructLayout(LayoutKind.Sequential)] struct ExtLimit {
        public BasicLimit basic; public IO io; public UIntPtr processMemory,jobMemory,peakProcess,peakJob;
    }
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateDirectoryW(string path,IntPtr attributes);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentProcessId();
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentThread();
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint GetProcessId(IntPtr p);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetProcessTimes(IntPtr p,out long created,out long exited,out long kernel,out long user);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll",SetLastError=true)] static extern IntPtr OpenProcess(uint access,bool inherit,uint pid);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool DuplicateHandle(IntPtr from,IntPtr source,IntPtr to,out IntPtr result,uint access,bool inherit,uint options);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessW(string app,StringBuilder command,IntPtr pa,IntPtr ta,bool inherit,uint flags,IntPtr env,string cwd,ref SI si,out PI pi);
    // Password is a pointer; it never crosses a managed string marshaller.
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool CreateProcessWithLogonW(string user,string domain,IntPtr password,uint logon,string app,StringBuilder command,uint flags,IntPtr env,string cwd,ref SI si,out PI pi);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint ResumeThread(IntPtr thread);
    [DllImport("kernel32.dll",SetLastError=true)] static extern uint WaitForSingleObject(IntPtr p,uint ms);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateProcess(IntPtr p,uint code);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetExitCodeProcess(IntPtr p,out uint code);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenProcessToken(IntPtr p,uint access,out IntPtr token);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool OpenThreadToken(IntPtr p,uint access,bool self,out IntPtr token);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool GetTokenInformation(IntPtr token,int kind,IntPtr data,uint capacity,out uint needed);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool IsValidSid(IntPtr sid);
    [DllImport("advapi32.dll")] static extern uint GetLengthSid(IntPtr sid);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool LookupPrivilegeValueW(string system,string name,out Luid value);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool DuplicateTokenEx(IntPtr token,uint access,IntPtr attrs,int level,int type,out IntPtr duplicate);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool CheckTokenMembership(IntPtr token,IntPtr sid,out bool result);
    [DllImport("userenv.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool GetUserProfileDirectoryW(IntPtr token,StringBuilder text,ref uint chars);
    [DllImport("shell32.dll")] static extern int SHGetKnownFolderPath(ref Guid id,uint flags,IntPtr token,out IntPtr path);
    [DllImport("ole32.dll")] static extern void CoTaskMemFree(IntPtr p);
    [DllImport("advapi32.dll")] static extern int RegOpenCurrentUser(uint access,out IntPtr key);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode)] static extern int RegOpenKeyExW(IntPtr key,string child,uint options,uint access,out IntPtr result);
    [DllImport("advapi32.dll")] static extern int RegCloseKey(IntPtr key);
    [DllImport("user32.dll")] static extern IntPtr GetProcessWindowStation();
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] static extern IntPtr GetThreadDesktop(uint tid);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern bool GetUserObjectInformationW(IntPtr h,int kind,byte[] data,uint length,out uint needed);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr OpenWindowStationW(string name,bool inherit,uint access);
    [DllImport("user32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr OpenDesktopW(string name,uint flags,bool inherit,uint access);
    [DllImport("user32.dll",SetLastError=true)] static extern bool CloseWindowStation(IntPtr h);
    [DllImport("user32.dll",SetLastError=true)] static extern bool CloseDesktop(IntPtr h);
    [DllImport("user32.dll",SetLastError=true)] static extern bool GetUserObjectSecurity(IntPtr h,ref uint info,byte[] data,uint capacity,out uint needed);
    [DllImport("user32.dll",SetLastError=true)] static extern bool SetUserObjectSecurity(IntPtr h,ref uint info,byte[] descriptor);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr OpenSCManagerW(string machine,string database,uint access);
    [DllImport("advapi32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr OpenServiceW(IntPtr scm,string name,uint access);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool QueryServiceStatusEx(IntPtr service,int level,byte[] data,uint size,out uint needed);
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool CloseServiceHandle(IntPtr h);
    [DllImport("netapi32.dll",CharSet=CharSet.Unicode)] static extern uint NetUserAdd(string server,uint level,ref User1 info,out uint parameter);
    [DllImport("netapi32.dll",CharSet=CharSet.Unicode)] static extern uint NetUserGetInfo(string server,string user,uint level,out IntPtr info);
    [DllImport("netapi32.dll")] static extern uint NetApiBufferSize(IntPtr p,out uint size);
    [DllImport("netapi32.dll")] static extern uint NetApiBufferFree(IntPtr p);
    [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern IntPtr CreateJobObjectW(IntPtr attrs,string name);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job,int kind,ref ExtLimit info,uint bytes);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job,IntPtr p);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool IsProcessInJob(IntPtr p,IntPtr job,out bool member);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job,int kind,byte[] data,uint bytes,out uint needed);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool TerminateJobObject(IntPtr job,uint code);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetNamedPipeClientProcessId(IntPtr pipe,out uint pid);
    [DllImport("kernel32.dll",SetLastError=true)] static extern bool GetNamedPipeServerProcessId(IntPtr pipe,out uint pid);

    static void Close(ref IntPtr h) { if(h!=IntPtr.Zero) { if(!CloseHandle(h)) throw Error("close-handle"); h=IntPtr.Zero; } }
    public static long CurrentCreated() { return Created(GetCurrentProcess()); }
    public static void FreshDirectory(string path) { Plain(path); if(!CreateDirectoryW(path,IntPtr.Zero)) throw Error("fresh-directory"); }
    public static void VerifyProcess(uint pid,long created) {
        IntPtr p=OpenProcess(0x1000,false,pid); if(p==IntPtr.Zero) throw Error("verify-process-open");
        try { Need(GetProcessId(p)==pid && Created(p)==created,"verify-process-identity"); } finally { KwilOrdinary.Close(ref p); }
    }
    static long Created(IntPtr p) { long c,e,k,u; if(!GetProcessTimes(p,out c,out e,out k,out u)) throw Error("process-times"); return c; }
    public static void Layout() {
        Need(IntPtr.Size==8 && Marshal.SizeOf<SI>()==104 && Marshal.SizeOf<PI>()==24 &&
          Marshal.SizeOf<Stats>()==56 && Marshal.SizeOf<SidAttr>()==16 &&
          Marshal.OffsetOf<GroupsOne>("first").ToInt32()==8 && Marshal.SizeOf<User1>()==56 &&
          Marshal.SizeOf<User23>()==40 && Marshal.SizeOf<ExtLimit>()==144,"native-layout");
    }
    sealed class Info : IDisposable {
        internal IntPtr data; internal uint bytes;
        internal Info(IntPtr token,int kind,uint fixedSize=0) {
            uint n=fixedSize;
            if(n==0) { bool ok=GetTokenInformation(token,kind,IntPtr.Zero,0,out n); int err=Marshal.GetLastWin32Error();
                Need(!ok && err==122 && n>=4 && n<=MaxBytes,"variable-token-probe"); }
            data=Marshal.AllocHGlobal(checked((int)n));
            try { if(!GetTokenInformation(token,kind,data,n,out bytes)) throw Error("token-read");
                Need(bytes==n && bytes>0,"token-read-size"); }
            catch { Dispose(); throw; }
        }
        internal string Sid(IntPtr p) {
            long offset=p.ToInt64()-data.ToInt64(); Need(offset>=0 && offset+8<=bytes,"sid-header");
            byte count=Marshal.ReadByte(p,1); Need(count<=15 && offset+8+4L*count<=bytes,"sid-extent");
            Need(IsValidSid(p) && GetLengthSid(p)==8+4U*count,"sid-valid");
            return new SecurityIdentifier(p).Value;
        }
        public void Dispose() { if(data!=IntPtr.Zero) { Marshal.FreeHGlobal(data); data=IntPtr.Zero; } }
    }
    static uint U32(IntPtr token,int kind) { using(var p=new Info(token,kind,4)) return unchecked((uint)Marshal.ReadInt32(p.data)); }
    public sealed class Facts {
        public string UserSid="",AuthenticationId="",LogonSid="",IntegritySid="",Profile="",Local="",Programs="",Station="",Desktop="";
        public uint SessionId,TokenElevation,TokenElevationType; public int TokenType;
        public bool AdminSidPresent,EffectiveAdmin,PowerfulPrivilegePresent,HkcuLoaded;
    }
    public const uint MeasurementTokenAccess=0x000e;
    public delegate bool TokenOpen(IntPtr process,uint access,out IntPtr token);
    public static IntPtr OpenMeasurementToken(IntPtr process,TokenOpen open) {
        IntPtr token;
        if(!open(process,MeasurementTokenAccess,out token)) throw Error("process-token");
        return token;
    }
    public delegate int FolderQuery(ref Guid id,uint flags,IntPtr token,out IntPtr path);
    public static string ReadFolder(IntPtr token,string guid,FolderQuery query,Func<IntPtr,string> read,Action<IntPtr> free) {
        var id=new Guid(guid); IntPtr p=IntPtr.Zero;
        try {
            int hr=query(ref id,0,token,out p);
            if(hr!=0) throw new InvalidOperationException("ordinary-user:known-folder");
            string s=read(p); Need(s!=null && s.Length>0 && s.Length<32767,"folder-length");
            return Path.GetFullPath(s).TrimEnd('\\');
        } finally { if(p!=IntPtr.Zero) free(p); }
    }
    static string Folder(IntPtr token,string guid) {
        return ReadFolder(token,guid,SHGetKnownFolderPath,Marshal.PtrToStringUni,CoTaskMemFree);
    }
    public static uint WaitNaturalCore(long deadline,Func<long> now,Func<uint,uint> wait,Action verify,Func<bool> forbidden,Func<uint> exitCode) {
        Need(now()<deadline,"natural-deadline");
        uint result=wait(checked((uint)Math.Min(Int32.MaxValue,Math.Max(0,deadline-now()))));
        verify();
        Need(now()<deadline && result==0 && !forbidden(),"natural-exit");
        uint code=exitCode();
        Need(now()<deadline,"natural-deadline");
        Need(code==0,"natural-nonzero");
        return code;
    }
    public static int MeasurementAndDeadlineControls() {
        int checks=0,opens=0,frees=0,reads=0;
        TokenOpen open=(IntPtr process,uint access,out IntPtr token)=>{
            opens++; Need(access==0x000e,"control-token-access"); token=new IntPtr(17); return true;
        };
        Need(OpenMeasurementToken(new IntPtr(1),open)==new IntPtr(17) && opens==1,"control-token-open"); checks++;
        FolderQuery failed=(ref Guid id,uint flags,IntPtr token,out IntPtr path)=>{path=new IntPtr(19);return -1;};
        bool refused=false;
        try { ReadFolder(IntPtr.Zero,Guid.Empty.ToString(),failed,p=>{reads++;return @"E:\fixture";},p=>{Need(p==new IntPtr(19),"control-free-owner");frees++;}); }
        catch(InvalidOperationException) { refused=true; }
        Need(refused && frees==1 && reads==0,"control-failed-folder-cleanup"); checks++;
        FolderQuery absent=(ref Guid id,uint flags,IntPtr token,out IntPtr path)=>{path=IntPtr.Zero;return -1;};
        try { ReadFolder(IntPtr.Zero,Guid.Empty.ToString(),absent,p=>@"E:\fixture",p=>{frees++;}); } catch(InvalidOperationException) {}
        Need(frees==1,"control-null-folder-no-free"); checks++;
        FolderQuery success=(ref Guid id,uint flags,IntPtr token,out IntPtr path)=>{path=new IntPtr(19);return 0;};
        Need(ReadFolder(IntPtr.Zero,Guid.Empty.ToString(),success,p=>@"E:\fixture",p=>{frees++;})==@"E:\fixture" && frees==2,"control-success-folder-cleanup"); checks++;
        refused=false;
        try { ReadFolder(IntPtr.Zero,Guid.Empty.ToString(),success,p=>{throw new InvalidOperationException("read");},p=>{frees++;}); }
        catch(InvalidOperationException) { refused=true; }
        Need(refused && frees==3,"control-read-failure-cleanup"); checks++;
        long clock=101; int waits=0,codes=0;
        Func<uint,uint> wait=remaining=>{waits++;return 0;};
        Func<uint> code=()=>{codes++;return 0;};
        refused=false;
        try { WaitNaturalCore(100,()=>clock,wait,()=>{},()=>false,code); } catch(InvalidOperationException) { refused=true; }
        Need(refused && waits==0 && codes==0,"control-expired-no-effects"); checks++;
        clock=99; refused=false;
        try { WaitNaturalCore(100,()=>clock,r=>{waits++;clock=101;return 0;},()=>{},()=>false,code); } catch(InvalidOperationException) { refused=true; }
        Need(refused && codes==0,"control-delayed-watchdog-late-signal"); checks++;
        clock=99; refused=false;
        try { WaitNaturalCore(100,()=>clock,wait,()=>{},()=>false,()=>{clock=100;return 0;}); } catch(InvalidOperationException) { refused=true; }
        Need(refused,"control-late-exit-query"); checks++;
        clock=90;
        Need(WaitNaturalCore(100,()=>clock,r=>{Need(r==10,"control-original-deadline");clock=99;return 0;},()=>{},()=>false,code)==0,"control-on-time-success"); checks++;
        refused=false;
        try { WaitNaturalCore(100,()=>clock,wait,()=>{},()=>true,code); } catch(InvalidOperationException) { refused=true; }
        Need(refused,"control-watchdog-failure-preserved"); checks++;
        return checks;
    }
    static Facts Measure(IntPtr process,bool current,bool profile) {
        IntPtr token=IntPtr.Zero;
        try {
            token=OpenMeasurementToken(process,OpenProcessToken);
            var f=new Facts();
            using(var p=new Info(token,1)) { Need(p.bytes>=16,"token-user"); f.UserSid=p.Sid(Marshal.ReadIntPtr(p.data)); }
            using(var p=new Info(token,10,56)) { var st=Marshal.PtrToStructure<Stats>(p.data);
                Need(st.groups<=MaxGroups && st.privileges<=256,"token-counts"); f.AuthenticationId=st.authentication.ToString(); f.TokenType=st.type; }
            f.SessionId=U32(token,12); f.TokenElevationType=U32(token,18); f.TokenElevation=U32(token,20);
            using(var p=new Info(token,25)) { Need(p.bytes>=16,"label"); f.IntegritySid=p.Sid(Marshal.ReadIntPtr(p.data)); }
            int logons=0;
            using(var p=new Info(token,2)) {
                uint count=unchecked((uint)Marshal.ReadInt32(p.data)); Need(count<=MaxGroups && 8L+16L*count<=p.bytes,"groups");
                for(int i=0;i<count;i++) { var g=Marshal.PtrToStructure<SidAttr>(IntPtr.Add(p.data,8+16*i)); string sid=p.Sid(g.sid);
                    if(sid=="S-1-5-32-544") f.AdminSidPresent=true;
                    if((g.attributes & 0xc0000000)==0xc0000000) { f.LogonSid=sid; logons++; } }
            }
            Need(logons==1,"logon-sid");
            using(var p=new Info(token,28)) { uint count=unchecked((uint)Marshal.ReadInt32(p.data)); Need(count==1 && p.bytes>=24,"logon-groups");
                Need(p.Sid(Marshal.ReadIntPtr(p.data,8))==f.LogonSid,"logon-sid-agreement"); }
            byte[] admin=new byte[68]; var adminSid=new SecurityIdentifier("S-1-5-32-544"); adminSid.GetBinaryForm(admin,0);
            IntPtr ap=Marshal.AllocHGlobal(adminSid.BinaryLength),duplicate=IntPtr.Zero;
            try { Marshal.Copy(admin,0,ap,adminSid.BinaryLength);
                if(!DuplicateTokenEx(token,8,IntPtr.Zero,2,2,out duplicate)) throw Error("membership-duplicate");
                if(!CheckTokenMembership(duplicate,ap,out f.EffectiveAdmin)) throw Error("membership");
            } finally { try { KwilOrdinary.Close(ref duplicate); } finally { Marshal.FreeHGlobal(ap); } }
            using(var p=new Info(token,3)) {
                uint count=unchecked((uint)Marshal.ReadInt32(p.data)); Need(count<=256 && 4L+12L*count<=p.bytes,"privileges");
                foreach(string name in new[]{"SeTcbPrivilege","SeCreateTokenPrivilege","SeAssignPrimaryTokenPrivilege","SeDebugPrivilege","SeBackupPrivilege","SeRestorePrivilege","SeImpersonatePrivilege","SeTakeOwnershipPrivilege","SeLoadDriverPrivilege","SeSecurityPrivilege"}) {
                    Luid required; if(!LookupPrivilegeValueW(null,name,out required)) throw Error("privilege-id");
                    for(int i=0;i<count;i++) { var value=Marshal.PtrToStructure<Luid>(IntPtr.Add(p.data,4+12*i));
                        if(value.low==required.low && value.high==required.high) f.PowerfulPrivilegePresent=true; }
                }
            }
            if(profile) {
                uint cap=32768; var text=new StringBuilder(32768);
                if(!GetUserProfileDirectoryW(token,text,ref cap)) throw Error("profile-directory");
                Need(cap>0 && cap<=32768,"profile-length"); f.Profile=Path.GetFullPath(text.ToString()).TrimEnd('\\');
                f.Local=Folder(token,"F1B32785-6FBA-4FCF-9D55-7B8E7F157091"); f.Programs=Folder(token,"A77F5D77-2E2B-44C3-A6A2-ABA601054A51");
                Need(f.Local.StartsWith(f.Profile+"\\",StringComparison.OrdinalIgnoreCase) &&
                    f.Programs.StartsWith(f.Profile+"\\",StringComparison.OrdinalIgnoreCase),"profile-known-folders");
                Plain(f.Profile); Plain(f.Local); Plain(f.Programs);
                if(current) {
                    IntPtr hkcu=IntPtr.Zero,hku=IntPtr.Zero;
                    try { Need(RegOpenCurrentUser(0x20019,out hkcu)==0,"hkcu-open");
                        Need(RegOpenKeyExW(new IntPtr(unchecked((int)0x80000003)),f.UserSid,0,0x20019,out hku)==0,"hku-profile");
                        f.HkcuLoaded=true;
                    } finally { int a=0,b=0; if(hku!=IntPtr.Zero) a=RegCloseKey(hku); if(hkcu!=IntPtr.Zero) b=RegCloseKey(hkcu); Need(a==0 && b==0,"profile-key-close"); }
                }
            }
            if(current) { f.Station=ObjectName(GetProcessWindowStation(),2,"WindowStation"); f.Desktop=ObjectName(GetThreadDesktop(GetCurrentThreadId()),2,"Desktop"); }
            return f;
        } finally { KwilOrdinary.Close(ref token); }
    }
    public static Facts Current(bool profile) {
        IntPtr thread; if(OpenThreadToken(GetCurrentThread(),8,true,out thread)) { KwilOrdinary.Close(ref thread); throw new InvalidOperationException("ordinary-user:thread-impersonation"); }
        Need(Marshal.GetLastWin32Error()==1008,"thread-token-error"); return Measure(GetCurrentProcess(),true,profile);
    }
    public static void Ordinary(Facts f) {
        Need(f.UserSid.StartsWith("S-1-5-21-") && !f.UserSid.EndsWith("-500") && f.TokenType==1 && f.SessionId!=0,"ordinary-user-primary-session");
        Need(f.TokenElevation==0 && f.TokenElevationType==1 && f.IntegritySid=="S-1-16-8192" &&
            !f.AdminSidPresent && !f.EffectiveAdmin && !f.PowerfulPrivilegePresent,"ordinary-medium-no-admin");
    }
    public static void Same(Facts a,Facts b) {
        Need(a.UserSid==b.UserSid && a.AuthenticationId==b.AuthenticationId && a.LogonSid==b.LogonSid && a.SessionId==b.SessionId,"same-identity");
        Need(a.TokenType==b.TokenType && a.TokenElevation==b.TokenElevation && a.TokenElevationType==b.TokenElevationType &&
            a.IntegritySid==b.IntegritySid && a.AdminSidPresent==b.AdminSidPresent && a.EffectiveAdmin==b.EffectiveAdmin &&
            a.PowerfulPrivilegePresent==b.PowerfulPrivilegePresent,"same-token-facts");
    }
    public static void Plain(string path) {
        Need(Path.IsPathFullyQualified(path) && path.IndexOfAny(new[]{'\0','\r','\n','"'})<0,"plain-path");
        string p=Path.GetFullPath(path);
        while(!String.IsNullOrEmpty(p)) {
            if(File.Exists(p) || Directory.Exists(p)) Need((File.GetAttributes(p)&FileAttributes.ReparsePoint)==0,"reparse-path");
            string next=Path.GetDirectoryName(p); if(next==p) break; p=next;
        }
    }
    static string ObjectName(IntPtr h,int kind,string type) {
        Need(h!=IntPtr.Zero,"object-handle"); uint n; byte[] data=new byte[512];
        if(!GetUserObjectInformationW(h,kind,data,512,out n)) throw Error("object-name");
        Need(n>=2 && n<=512 && n%2==0,"object-name-bound"); string s=Encoding.Unicode.GetString(data,0,checked((int)n-2));
        Need(s.IndexOf('\0')<0,"object-name-nul");
        if(kind==2) Need(ObjectName(h,3,type)==type,"object-type");
        return s;
    }
    public static void ServiceReady() {
        IntPtr scm=OpenSCManagerW(null,null,1),service=IntPtr.Zero;
        if(scm==IntPtr.Zero) throw Error("scm-query");
        try { service=OpenServiceW(scm,"seclogon",4); if(service==IntPtr.Zero) throw Error("seclogon-query");
            uint n; byte[] bytes=new byte[36]; if(!QueryServiceStatusEx(service,0,bytes,36,out n)) throw Error("seclogon-status");
            Need(n==36 && BitConverter.ToUInt32(bytes,4)==4,"seclogon-not-running");
        } finally { bool a=true,b=true; if(service!=IntPtr.Zero) a=CloseServiceHandle(service); b=CloseServiceHandle(scm); Need(a && b,"service-handle-close"); }
    }

    public sealed class Cleanup {
        public bool Exited,ProcessClosed,ThreadClosed,DeadlineExpired,TerminationAttempted,IdentityVerified;
        public uint ExitCode; public string Failure="";
        public bool Success { get { return Exited && ProcessClosed && ThreadClosed && Failure==""; } }
    }
    // A preallocated owner receives native out-PI directly. Timer never inspects
    // the struct until the release/acquire ApiReturned publication.
    internal interface ILeaseCalls {
        long Now {get;}
        IDisposable Arm(Action tick,long deadline);
        uint Pid(IntPtr handle);
        long Created(IntPtr handle);
        uint Wait(IntPtr handle,uint milliseconds);
        bool Terminate(IntPtr handle,uint code);
        bool ExitCode(IntPtr handle,out uint code);
        uint Resume(IntPtr handle);
        IntPtr OpenTarget(uint pid);
        bool Duplicate(IntPtr source,IntPtr target,out IntPtr copy);
        bool Close(IntPtr handle);
    }
    sealed class SystemLeaseCalls : ILeaseCalls {
        public long Now {get {return Environment.TickCount64;}}
        public IDisposable Arm(Action tick,long deadline) {return new Timer(_=>tick(),null,checked((int)Math.Min(Int32.MaxValue,Math.Max(1,deadline-Now))),Timeout.Infinite);}
        public uint Pid(IntPtr h) {return GetProcessId(h);}
        public long Created(IntPtr h) {return KwilOrdinary.Created(h);}
        public uint Wait(IntPtr h,uint ms) {return WaitForSingleObject(h,ms);}
        public bool Terminate(IntPtr h,uint code) {return TerminateProcess(h,code);}
        public bool ExitCode(IntPtr h,out uint code) {return GetExitCodeProcess(h,out code);}
        public uint Resume(IntPtr h) {return ResumeThread(h);}
        public IntPtr OpenTarget(uint pid) {return OpenProcess(0x1040,false,pid);}
        public bool Duplicate(IntPtr source,IntPtr target,out IntPtr copy) {return DuplicateHandle(GetCurrentProcess(),source,target,out copy,0,false,2);}
        public bool Close(IntPtr h) {return CloseHandle(h);}
    }
    sealed class FakeLeaseCalls : ILeaseCalls {
        public long Clock=10; public long Now {get {return Clock;}}
        public Action Callback; public bool Disposed; public int Waits,Terminations,Duplicates;
        public bool Signaled=true,FailThreadCopy,FailClose,AdvanceWait;
        public uint Code=0; public readonly System.Collections.Generic.List<long> Closed=new System.Collections.Generic.List<long>();
        sealed class Alarm : IDisposable {readonly FakeLeaseCalls owner; internal Alarm(FakeLeaseCalls o){owner=o;} public void Dispose(){owner.Disposed=true;}}
        public IDisposable Arm(Action tick,long deadline){Callback=tick;return new Alarm(this);}
        public uint Pid(IntPtr h){return h.ToInt64()==71?7U:5U;}
        public long Created(IntPtr h){return 42;}
        public uint Wait(IntPtr h,uint ms){Waits++;if(AdvanceWait)Clock=101;return Signaled?0U:258U;}
        public bool Terminate(IntPtr h,uint code){Terminations++;Signaled=true;return true;}
        public bool ExitCode(IntPtr h,out uint code){code=Code;return true;}
        public uint Resume(IntPtr h){return 1;}
        public IntPtr OpenTarget(uint pid){return new IntPtr(71);}
        public bool Duplicate(IntPtr source,IntPtr target,out IntPtr copy){Duplicates++;copy=new IntPtr(80+Duplicates);return !(FailThreadCopy && Duplicates==2);}
        public bool Close(IntPtr h){if(FailClose && h.ToInt64()==52)return false;Closed.Add(h.ToInt64());return true;}
    }
    static Lease FakeOwnedLease(FakeLeaseCalls calls,bool publish=true) {
        var owner=new Lease(100,false,calls); owner.ApiEntered=true;
        owner.pi=new PI {process=new IntPtr(51),thread=new IntPtr(52),pid=5,tid=6};
        if(publish)owner.Publish(); return owner;
    }
    public static int LeaseEffectControls() {
        int checks=0; bool refused;
        var c=new FakeLeaseCalls(); var o=new Lease(100,false,c);
        c.Clock=101;c.Callback();
        Need(!o.LaunchAmbiguous && c.Waits==0,"control-timer-before-entry");checks++;
        o.ApiEntered=true;c.Callback();
        Need(o.LaunchAmbiguous && c.Waits==0,"control-timer-inflight");checks++;
        var pending=o.Close();
        Need(!pending.Success && o.LaunchAmbiguous && c.Closed.Count==0 && !c.Disposed,"control-inflight-close-no-inference");checks++;
        o.pi=new PI {process=new IntPtr(51),thread=new IntPtr(52),pid=5,tid=6};o.Publish();
        refused=false;try{o.WaitNatural();}catch(InvalidOperationException){refused=true;}
        Need(refused && c.Waits==0,"control-late-return-no-success");checks++;
        o.Close();Need(c.Closed.SequenceEqual(new long[]{52,51}) && c.Disposed,"control-late-known-handles-close");checks++;
        c=new FakeLeaseCalls();o=FakeOwnedLease(c,false);c.Clock=101;c.Callback();
        Need(o.LaunchAmbiguous && c.Waits==0 && c.Closed.Count==0,"control-PI-before-publication-not-queryable");checks++;
        o.Publish();o.Close();Need(c.Closed.SequenceEqual(new long[]{52,51}),"control-published-PI-cleaned");checks++;
        c=new FakeLeaseCalls {Signaled=false};o=FakeOwnedLease(c);c.Clock=101;c.Callback();
        Need(c.Terminations==1 && o.Result.TerminationAttempted && o.Result.Exited,"control-timer-after-publication");checks++;
        o.Close();Need(c.Closed.SequenceEqual(new long[]{52,51}),"control-timer-handles");checks++;
        c=new FakeLeaseCalls {AdvanceWait=true};o=FakeOwnedLease(c);refused=false;
        try{o.WaitNatural();}catch(InvalidOperationException){refused=true;}
        Need(refused && !o.Result.Exited,"control-wait-late-before-callback");checks++;o.Close();
        c=new FakeLeaseCalls {Signaled=false};o=FakeOwnedLease(c);refused=false;
        try{o.WaitNatural();}catch(InvalidOperationException){refused=true;}
        Need(refused && !o.Result.Exited && !o.Result.TerminationAttempted,"control-live-anchor-cannot-succeed");checks++;
        o.Close();Need(o.Result.TerminationAttempted && c.Terminations==1,"control-forced-cleanup-not-natural-exit");checks++;
        c=new FakeLeaseCalls {Code=7};o=FakeOwnedLease(c);refused=false;
        try{o.WaitNatural();}catch(InvalidOperationException){refused=true;}
        Need(refused && o.Result.ExitCode==7,"control-nonzero-exit");checks++;o.Close();
        c=new FakeLeaseCalls {FailThreadCopy=true};o=FakeOwnedLease(c);refused=false;
        try{o.DuplicateTo(7,42);}catch(Win32Exception){refused=true;}
        Need(refused && o.LaunchAmbiguous && c.Duplicates==2 && c.Closed.SequenceEqual(new long[]{71}),"control-partial-duplicate-retained");checks++;
        o.Close();Need(c.Closed.SequenceEqual(new long[]{71,52,51}),"control-partial-duplicate-local-cleanup");checks++;
        c=new FakeLeaseCalls();o=FakeOwnedLease(c);
        var transfer=o.DuplicateTo(7,42);
        Need(transfer.SequenceEqual(new[]{"81","82","5","6","42","100"}) && c.Closed.SequenceEqual(new long[]{71}),"control-original-transfer");checks++;
        // Lost acknowledgment/receipt IO cannot consume the original local PI.
        try{throw new IOException("mock-receipt");}catch(IOException){o.Close();}
        Need(c.Closed.SequenceEqual(new long[]{71,52,51}),"control-receipt-failure-local-cleanup");checks++;
        c=new FakeLeaseCalls {FailClose=true};o=FakeOwnedLease(c);var failed=o.Close();
        Need(!failed.Success && !failed.ThreadClosed && failed.ProcessClosed && o.pi.thread==new IntPtr(52) && !c.Disposed,"control-failed-close-retains-handle");checks++;
        c.FailClose=false;o.Close();
        Need(c.Closed.SequenceEqual(new long[]{51,52}) && c.Disposed && failed.Failure!="","control-close-retry-stays-failed");checks++;
        c=new FakeLeaseCalls();o=FakeOwnedLease(c);
        Need(o.WaitNatural()==0 && o.Close().Success && c.Closed.SequenceEqual(new long[]{52,51}),"control-natural-exact-cleanup");checks++;
        return checks;
    }
    public sealed class Lease : IDisposable {
        readonly object gate=new object(); readonly IDisposable timer; readonly ILeaseCalls calls;
        internal PI pi; int apiReturned; readonly long deadline;
        long cleanupEnd; bool cleanupBegun,closed; public readonly Cleanup Result=new Cleanup();
        public bool ApiEntered,ApiCompleted,LaunchAmbiguous;
        internal Lease(long absoluteDeadline,bool adopting=false) : this(absoluteDeadline,adopting,new SystemLeaseCalls()) {}
        internal Lease(long absoluteDeadline,bool adopting,ILeaseCalls effects) {
            calls=effects;
            Need(absoluteDeadline>0 && (adopting || absoluteDeadline>calls.Now),"lease-deadline"); deadline=absoluteDeadline;
            timer=calls.Arm(Tick,deadline);
        }
        public uint Pid { get { return pi.pid; } }
        public uint ThreadId { get { return pi.tid; } }
        public long Deadline { get { return deadline; } }
        public long Remaining { get { return Math.Max(0,deadline-calls.Now); } }
        public long CreatedAt { get { lock(gate) { Verify(); return calls.Created(pi.process); } } }
        void Note(string s) { if(Result.Failure.Length<1024) Result.Failure+=(Result.Failure==""?"":";")+s; }
        internal void Publish() { ApiCompleted=true; Volatile.Write(ref apiReturned,1); }
        void Verify() {
            Need(!closed && pi.process!=IntPtr.Zero && pi.pid!=0,"owned-process");
            Need(calls.Pid(pi.process)==pi.pid,"owned-pid"); Result.IdentityVerified=true;
        }
        void BeginCleanup() {
            if(!cleanupBegun) { cleanupEnd=Math.Min(deadline+10000,calls.Now+10000); cleanupBegun=true; }
        }
        uint CleanupWait { get { return checked((uint)Math.Min(5000,Math.Max(0,cleanupEnd-calls.Now))); } }
        void StopKnown() {
            BeginCleanup(); Verify();
            uint status=calls.Wait(pi.process,0);
            if(status==258 && calls.Now<cleanupEnd) {
                Result.TerminationAttempted=true;
                if(!calls.Terminate(pi.process,0xe031)) Note("terminate-failed");
                status=calls.Wait(pi.process,CleanupWait);
            }
            Result.Exited=status==0;
            if(Result.Exited) { uint code; if(calls.ExitCode(pi.process,out code)) Result.ExitCode=code; else Note("exit-code"); }
            else Note("unreaped");
        }
        internal void Tick() {
            lock(gate) {
                if(closed) return; Result.DeadlineExpired=true;
                // An in-flight broker call is NOT cancelled or proven childless.
                if(Volatile.Read(ref apiReturned)==0) { LaunchAmbiguous=ApiEntered; return; }
                try { if(pi.process!=IntPtr.Zero) StopKnown(); } catch { Note("watchdog-cleanup"); }
            }
        }
        public Facts Suspended() { lock(gate) { Verify(); Need(Remaining>0 && !Result.DeadlineExpired,"suspended-deadline"); return Measure(pi.process,false,true); } }
        public void Resume() {
            lock(gate) { Verify(); Need(Remaining>0 && !Result.DeadlineExpired,"resume-deadline");
                Need(calls.Resume(pi.thread)==1,"resume-count"); }
        }
        public uint WaitNatural() {
            return WaitNaturalCore(deadline,()=>calls.Now,
                remaining=>calls.Wait(pi.process,remaining),
                ()=>{ lock(gate) { Verify(); } },
                ()=>{ lock(gate) { return Result.DeadlineExpired || Result.TerminationAttempted; } },
                ()=>{ lock(gate) {
                    Verify(); uint code; if(!calls.ExitCode(pi.process,out code)) throw Error("exit-code");
                    Result.Exited=true; Result.ExitCode=code; return code;
                } });
        }
        public string[] DuplicateTo(uint pid,long created) {
            lock(gate) {
                Verify(); IntPtr target=calls.OpenTarget(pid),p=IntPtr.Zero,t=IntPtr.Zero;
                if(target==IntPtr.Zero) throw Error("supervisor-open");
                try {
                    Need(calls.Pid(target)==pid && calls.Created(target)==created,"supervisor-identity");
                    if(!calls.Duplicate(pi.process,target,out p)) throw Error("duplicate-process");
                    if(!calls.Duplicate(pi.thread,target,out t)) {
                        // Remote ownership is now ambiguous; never guess a close
                        // after a possible IPC acknowledgment. S exit/VM disposal
                        // owns any unreported duplicate; result cannot be success.
                        LaunchAmbiguous=true; throw Error("duplicate-thread-ambiguous");
                    }
                    return new[]{p.ToInt64().ToString(),t.ToInt64().ToString(),pi.pid.ToString(),pi.tid.ToString(),calls.Created(pi.process).ToString(),deadline.ToString()};
                } finally { CloseOwned(ref target); }
            }
        }
        void CloseOwned(ref IntPtr handle) {
            if(handle==IntPtr.Zero) return;
            if(!calls.Close(handle)) throw Error("close-handle");
            handle=IntPtr.Zero;
        }
        public Cleanup Close() {
            lock(gate) {
                if(closed) return Result;
                if(Volatile.Read(ref apiReturned)==0 && ApiEntered) { LaunchAmbiguous=true; Note("inflight"); return Result; }
                try {
                    if(pi.process!=IntPtr.Zero) StopKnown();
                    else { Result.Exited=!ApiEntered; if(ApiEntered) Note("unknown-process"); }
                } catch { Note("reap-failed"); }
                // Try both closers; a failed close retains its exact handle.
                try { CloseOwned(ref pi.thread); Result.ThreadClosed=pi.thread==IntPtr.Zero; } catch { Note("thread-close"); }
                try { CloseOwned(ref pi.process); Result.ProcessClosed=pi.process==IntPtr.Zero; } catch { Note("process-close"); }
                closed=pi.process==IntPtr.Zero && pi.thread==IntPtr.Zero;
                if(closed) timer.Dispose();
                return Result;
            }
        }
        public void Dispose() { Close(); }
    }
    public static Lease PendingSame,PendingCredential;
    static SI Startup(out IntPtr desktop) {
        desktop=Marshal.StringToHGlobalUni("WinSta0\\Default");
        return new SI { cb=104,desktop=desktop };
    }
    public static Lease SameLaunch(string app,string command,string cwd,long deadline,int cap) {
        Need(cap>0 && cap<=300000 && deadline>Environment.TickCount64 && deadline<=Environment.TickCount64+cap,"same-lifetime"); Plain(app); Plain(cwd);
        Need(command.Length<1024,"same-command-length");
        var lease=new Lease(deadline); PendingSame=lease; IntPtr desk=IntPtr.Zero;
        try {
            var si=Startup(out desk); lease.ApiEntered=true;
            bool ok=CreateProcessW(app,new StringBuilder(command),IntPtr.Zero,IntPtr.Zero,false,CreationFlags,IntPtr.Zero,cwd,ref si,out lease.pi);
            lease.Publish();
            if(!ok) { lease.LaunchAmbiguous=true; throw Error("same-create"); }
            Need(lease.pi.process!=IntPtr.Zero && lease.pi.thread!=IntPtr.Zero,"same-return-handles");
            return lease;
        } finally { if(desk!=IntPtr.Zero) Marshal.FreeHGlobal(desk); }
    }
    public static Lease AdoptRemote(long p,long t,uint pid,uint tid,long created,long deadline) {
        // Even an expired transfer must first enter an owner, never leak by
        // refusing its deadline before adoption. It cannot renew proof time.
        var lease=new Lease(deadline,true); PendingCredential=lease;
        lease.pi=new PI { process=new IntPtr(p),thread=new IntPtr(t),pid=pid,tid=tid };
        lease.ApiEntered=true; lease.Publish();
        // Adopt BEFORE inspecting. Caller retains the owner even when Verify fails.
        PendingCredential=lease;
        Need(deadline>Environment.TickCount64 && deadline<=Environment.TickCount64+90000,"remote-deadline");
        Need(lease.CreatedAt==created,"remote-created"); return lease;
    }
    public static uint ReadJobActive(Func<byte[],uint> query) {
        byte[] bytes=new byte[48];uint n=query(bytes);
        Need(n==48,"job-accounting-size");
        return BitConverter.ToUInt32(bytes,40);
    }
    public static void AssignJobEffects(Func<bool> member,Action assign) {
        if(!member())assign();
        Need(member(),"job-not-contained");
    }
    public static void InstallPair(Action station,Action desktop) {station();desktop();}
    public static int AssignmentAndPairControls(DaclState original,DaclState expected) {
        int checks=0,assigns=0;bool member=false,refused=false;
        AssignJobEffects(()=>member,()=>{assigns++;member=true;});
        Need(member && assigns==1,"control-actual-job-assignment");checks++;
        AssignJobEffects(()=>member,()=>{assigns++;});
        Need(assigns==1,"control-contained-no-reassignment");checks++;
        member=false;
        try{AssignJobEffects(()=>member,()=>{assigns++;throw new IOException("assign");});}catch(IOException){refused=true;}
        Need(refused && !member,"control-assignment-failure-no-resume");checks++;
        refused=false;
        try{AssignJobEffects(()=>member,()=>{assigns++;});}catch(InvalidOperationException){refused=true;}
        Need(refused && !member,"control-membership-readback-failure");checks++;
        DaclState station=original,desktop=original;bool s=false,d=false;int sw=0,dw=0;refused=false;
        try {InstallPair(
            ()=>InstallDacl(original,expected,()=>station,v=>{sw++;station=v;},ref s),
            ()=>InstallDacl(original,expected,()=>desktop,v=>{dw++;throw new IOException("desktop-write");},ref d));}
        catch(IOException){refused=true;}
        Need(refused && s && d && sw==1 && dw==1 && DaclState.Equal(station,expected) && DaclState.Equal(desktop,original),"control-partial-two-object-grant");checks++;
        RestoreDacl(original,expected,d,()=>desktop,v=>{dw++;desktop=v;});
        RestoreDacl(original,expected,s,()=>station,v=>{sw++;station=v;});
        Need(sw==2 && dw==1 && DaclState.Equal(station,original) && DaclState.Equal(desktop,original),"control-partial-pair-reverse-restore");checks++;
        Need(ReadJobActive(b=>{BitConverter.GetBytes(1U).CopyTo(b,40);return 48;})==1,"control-extra-child-visible");checks++;
        Need(ReadJobActive(b=>48)==0,"control-empty-owned-job");checks++;
        refused=false;try{ReadJobActive(b=>47);}catch(InvalidOperationException){refused=true;}
        Need(refused,"control-job-accounting-size-refusal");checks++;
        return checks;
    }
    public sealed class Job : IDisposable {
        IntPtr handle;
        public Job() {
            handle=CreateJobObjectW(IntPtr.Zero,null); if(handle==IntPtr.Zero) throw Error("job-create");
            var value=new ExtLimit(); value.basic.flags=0x2008; value.basic.active=8; // kill-on-close, cap; NO breakaway/UI limits
            if(!SetInformationJobObject(handle,9,ref value,144)) { int error=Marshal.GetLastWin32Error(); Dispose(); throw new Win32Exception(error,"ordinary-user:job-limits"); }
        }
        public void Assign(Lease owner) {
            AssignJobEffects(()=>{
                bool member;if(!IsProcessInJob(owner.pi.process,handle,out member))throw Error("job-membership");
                return member;
            },()=>{if(!AssignProcessToJobObject(handle,owner.pi.process))throw Error("job-assign");});
        }
        public uint Active {
            get { return ReadJobActive(b=>{
                uint n;if(!QueryInformationJobObject(handle,1,b,48,out n))throw Error("job-accounting");
                return n;
            }); }
        }
        public void Stop() { if(handle!=IntPtr.Zero && !TerminateJobObject(handle,0xe032)) throw Error("owned-job-stop"); }
        public void Dispose() { KwilOrdinary.Close(ref handle); }
    }
    public sealed class Wire : IDisposable {
        readonly PipeStream pipe; public Wire(PipeStream stream) { pipe=stream; }
        public uint Peer(bool server) {
            uint pid; bool ok=server?GetNamedPipeClientProcessId(pipe.SafePipeHandle.DangerousGetHandle(),out pid):GetNamedPipeServerProcessId(pipe.SafePipeHandle.DangerousGetHandle(),out pid);
            if(!ok) throw Error("pipe-peer"); return pid;
        }
        static void Wait(Task task,int ms) { Need(ms>0 && task.Wait(ms),"pipe-deadline"); task.GetAwaiter().GetResult(); }
        void ReadAll(byte[] b,int ms) {
            long end=Environment.TickCount64+ms; int done=0;
            while(done<b.Length) { int left=checked((int)Math.Max(0,end-Environment.TickCount64));
                var task=pipe.ReadAsync(b,done,b.Length-done); Wait(task,left);
                int n=task.Result; Need(n>0,"pipe-eof"); done+=n; }
        }
        public string Read(int ms) {
            long end=Environment.TickCount64+ms; byte[] size=new byte[4]; ReadAll(size,ms);
            int n=BitConverter.ToInt32(size,0); Need(n>0 && n<=4096,"pipe-frame-size");
            byte[] b=new byte[n]; ReadAll(b,checked((int)Math.Max(0,end-Environment.TickCount64))); return new UTF8Encoding(false,true).GetString(b);
        }
        public void Write(string text,int ms) {
            byte[] b=new UTF8Encoding(false,true).GetBytes(text); Need(b.Length>0 && b.Length<=4096,"pipe-frame-size");
            byte[] frame=new byte[b.Length+4]; BitConverter.GetBytes(b.Length).CopyTo(frame,0); b.CopyTo(frame,4);
            Wait(pipe.WriteAsync(frame,0,frame.Length),ms);
        }
        public void Dispose() { pipe.Dispose(); }
    }
    public static void CredentialSetupEffects(Action create,Action querySid,Action dispose) {
        try {create();querySid();} catch {dispose();throw;}
    }
    public static void CredentialLaunchEffects(Func<bool> api,Action publish,Action validate,Action dispose,Action failure=null) {
        try {bool ok=api();publish();if(!ok){if(failure!=null)failure();Need(false,"credential-create");}validate();}
        finally {dispose();}
    }
    public static void ClearCredential(ref IntPtr pointer,Action<IntPtr> zero,Action<IntPtr> free) {
        if(pointer==IntPtr.Zero)return;
        zero(pointer);free(pointer);pointer=IntPtr.Zero;
    }
    public static int CredentialEffectControls() {
        int checks=0,created=0,queried=0,disposed=0; bool refused=false;
        try {CredentialSetupEffects(()=>{throw new IOException("create");},()=>{queried++;},()=>{disposed++;});}
        catch(IOException){refused=true;}
        Need(refused && queried==0 && disposed==1,"control-create-failure-secret-cleanup");checks++;
        refused=false;
        try {CredentialSetupEffects(()=>{created++;},()=>{queried++;throw new IOException("sid");},()=>{disposed++;});}
        catch(IOException){refused=true;}
        Need(refused && created==1 && queried==1 && disposed==2,"control-SID-failure-secret-cleanup");checks++;
        CredentialSetupEffects(()=>{created++;},()=>{queried++;},()=>{disposed++;});
        Need(created==2 && queried==2 && disposed==2,"control-setup-retains-secret-for-logon");checks++;
        bool inflight=false;int published=0,validated=0;
        Action cleanup=()=>{Need(!inflight,"control-secret-still-in-flight");disposed++;};
        CredentialLaunchEffects(()=>{inflight=true;Need(disposed==2 && published==0,"control-no-premature-secret-dispose");inflight=false;return true;},
            ()=>{published++;},()=>{validated++;},cleanup);
        Need(published==1 && validated==1 && disposed==3,"control-API-return-publish-before-dispose");checks++;
        refused=false;
        try {CredentialLaunchEffects(()=>false,()=>{published++;},()=>{validated++;},cleanup);}
        catch(InvalidOperationException){refused=true;}
        Need(refused && published==2 && validated==1 && disposed==4,"control-failed-API-published-and-cleared");checks++;
        refused=false;
        try {CredentialLaunchEffects(()=>{throw new IOException("interrupted-return");},()=>{published++;},()=>{validated++;},cleanup);}
        catch(IOException){refused=true;}
        Need(refused && published==2 && disposed==5,"control-no-false-return-publication");checks++;
        refused=false;
        try {CredentialLaunchEffects(()=>true,()=>{published++;throw new IOException("publish");},()=>{validated++;},cleanup);}
        catch(IOException){refused=true;}
        Need(refused && validated==1 && disposed==6,"control-publication-failure-cleanup");checks++;
        refused=false;
        try {CredentialLaunchEffects(()=>true,()=>{published++;},()=>{validated++;throw new IOException("inspect");},cleanup);}
        catch(IOException){refused=true;}
        Need(refused && disposed==7,"control-post-adoption-failure-cleanup");checks++;
        var pointer=new IntPtr(23);var events=new System.Collections.Generic.List<string>();
        ClearCredential(ref pointer,p=>{Need(p==new IntPtr(23),"control-secret-owner");events.Add("zero");},
            p=>{Need(events.SequenceEqual(new[]{"zero"}),"control-zero-before-free");events.Add("free");});
        Need(pointer==IntPtr.Zero && events.SequenceEqual(new[]{"zero","free"}),"control-clear-exact-owner");checks++;
        ClearCredential(ref pointer,p=>{throw new IOException("double-zero");},p=>{throw new IOException("double-free");});
        Need(events.Count==2,"control-idempotent-clear");checks++;
        pointer=new IntPtr(23);refused=false;
        try {ClearCredential(ref pointer,p=>{throw new IOException("zero");},p=>{events.Add("unexpected-free");});}
        catch(IOException){refused=true;}
        Need(refused && pointer==new IntPtr(23) && events.Count==2,"control-zero-refusal-retains-owner");checks++;
        refused=false;
        try {ClearCredential(ref pointer,p=>{events.Add("zero");},p=>{throw new IOException("free");});}
        catch(IOException){refused=true;}
        Need(refused && pointer==new IntPtr(23),"control-free-refusal-retains-owner");checks++;
        return checks;
    }
    public sealed unsafe class Credential : IDisposable {
        public const int Characters=64, Bytes=130; IntPtr pointer;
        public string User="",Sid=""; public bool CreateReturned,Created,SidKnown,Zeroed,Freed;
        public Credential(string name,string marker) {
            Need(name.Length==15 && name.StartsWith("kwo") && name.Skip(3).All(c=>"0123456789abcdef".Contains(c)),"account-name");
            User=name; pointer=Marshal.AllocHGlobal(Bytes);
            CredentialSetupEffects(()=>{
                var span=new Span<byte>((void*)pointer,Bytes); RandomNumberGenerator.Fill(span);
                const string pool="ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!@#$%*-_";
                char* chars=(char*)pointer;
                for(int i=0;i<Characters;i++) chars[i]=pool[((byte*)pointer)[2*i]%pool.Length];
                // Fixed complexity classes without ever constructing a password string.
                chars[0]='A'; chars[1]='a'; chars[2]='7'; chars[3]='!'; chars[Characters]='\0';
                IntPtr un=IntPtr.Zero,comment=IntPtr.Zero;
                try { un=Marshal.StringToHGlobalUni(name);comment=Marshal.StringToHGlobalUni(marker);var data=new User1 { name=un,password=pointer,privilege=1,comment=comment,flags=0x201 };
                    uint parameter; uint result=NetUserAdd(null,1,ref data,out parameter); CreateReturned=true;
                    Need(result==0,"account-create-status-"+result.ToString()); Created=true;
                } finally { if(un!=IntPtr.Zero)Marshal.FreeHGlobal(un);if(comment!=IntPtr.Zero)Marshal.FreeHGlobal(comment); }
            },()=>{
                IntPtr info; uint rc=NetUserGetInfo(null,name,23,out info); Need(rc==0 && info!=IntPtr.Zero,"created-account-query");
                try { uint n; Need(NetApiBufferSize(info,out n)==0 && n>=40 && n<=MaxBytes,"account-info-size");
                    var data=Marshal.PtrToStructure<User23>(info); long at=data.sid.ToInt64()-info.ToInt64();
                    Need(at>=0 && at+8<=n,"account-sid-header"); byte count=Marshal.ReadByte(data.sid,1);
                    Need(count<=15 && at+8+4L*count<=n && IsValidSid(data.sid),"account-sid-size");
                    Sid=new SecurityIdentifier(data.sid).Value; Need(Sid.StartsWith("S-1-5-21-") && !Sid.EndsWith("-500"),"created-account-sid");
                    SidKnown=true;
                } finally { Need(NetApiBufferFree(info)==0,"account-buffer-free"); }
            },Dispose);
        }
        public Lease Launch(string app,string command,string cwd,string machine,long deadline) {
            Need(pointer!=IntPtr.Zero && Created && SidKnown,"credential-owner");
            Plain(app); Plain(cwd); Need(command.Length<1024,"logon-command-length");
            Need(deadline>Environment.TickCount64 && deadline<=Environment.TickCount64+90000,"credential-deadline");
            var lease=new Lease(deadline); PendingCredential=lease; IntPtr desktop=IntPtr.Zero;int launchError=0;
            CredentialLaunchEffects(()=>{
                ServiceReady(); var si=Startup(out desktop); lease.ApiEntered=true;
                bool ok=CreateProcessWithLogonW(User,machine,pointer,1,app,new StringBuilder(command),CreationFlags,IntPtr.Zero,cwd,ref si,out lease.pi);
                if(!ok){launchError=Marshal.GetLastWin32Error();lease.LaunchAmbiguous=true;}
                return ok;
            },()=>{
                lease.Publish(); // PI is owned before zeroing, inspection or logging.
            },()=>{
                Need(lease.pi.process!=IntPtr.Zero && lease.pi.thread!=IntPtr.Zero,"credential-return-handles");
            },()=>{ if(desktop!=IntPtr.Zero) Marshal.FreeHGlobal(desktop); Dispose(); },()=>{throw new Win32Exception(launchError,"ordinary-user:credential-create");});
            return lease;
        }
        public void Dispose() {
            ClearCredential(ref pointer,p=>{
                CryptographicOperations.ZeroMemory(new Span<byte>((void*)p,Bytes)); Zeroed=true;
            },p=>Marshal.FreeHGlobal(p));
            Freed=pointer==IntPtr.Zero;
        }
    }

    [StructLayout(LayoutKind.Sequential)] struct Mapping { public uint read,write,execute,all; }
    [DllImport("advapi32.dll",SetLastError=true)] static extern bool AccessCheck(byte[] descriptor,IntPtr token,uint desired,ref Mapping mapping,byte[] privileges,ref uint bytes,out uint granted,out bool allowed);
    public const uint DaclWriteInformation=4;
    public sealed class DaclState {
        internal ushort control; internal byte[] acl;
        public string Hash { get { return Convert.ToHexString(SHA256.HashData(EncodingBytes())).ToLowerInvariant(); } }
        public int Bytes { get { return acl.Length; } }
        internal byte[] EncodingBytes() { byte[] b=new byte[acl.Length+2]; BitConverter.GetBytes(control).CopyTo(b,0); acl.CopyTo(b,2); return b; }
        internal byte[] Descriptor() {
            byte[] sd=new byte[20+acl.Length]; sd[0]=1;
            BitConverter.GetBytes((ushort)(0x8000|control)).CopyTo(sd,2);
            BitConverter.GetBytes(20U).CopyTo(sd,16); acl.CopyTo(sd,20); return sd;
        }
        public static bool Equal(DaclState a,DaclState b) { return a.control==b.control && a.acl.SequenceEqual(b.acl); }
    }
    static string BinarySid(byte[] data,int at,int end) {
        Need(at>=0 && at+8<=end && end<=data.Length && data[at]==1 && data[at+1]<=15,"descriptor-sid-header");
        Need(at+8+4*data[at+1]<=end,"descriptor-sid-size"); return new SecurityIdentifier(data,at).Value;
    }
    public static DaclState ParseDacl(byte[] sd) {
        Need(sd!=null && sd.Length>=28 && sd.Length<=MaxBytes && sd[0]==1,"descriptor-size-revision");
        ushort control=BitConverter.ToUInt16(sd,2);
        Need((control&0x8004)==0x8004 && (control&0x0108)==0 && (control&~0x9407)==0,"descriptor-control"); // present, relative, not defaulted/pending
        uint offset=BitConverter.ToUInt32(sd,16); Need(offset>=20 && offset%4==0 && offset+8<=sd.Length,"dacl-offset");
        int at=checked((int)offset),size=BitConverter.ToUInt16(sd,at+2),count=BitConverter.ToUInt16(sd,at+4);
        Need(sd[at]==2 && size>=8 && at+size<=sd.Length && count<=MaxAces,"acl-layout");
        byte[] acl=new byte[size]; Array.Copy(sd,at,acl,0,size);
        int cursor=8; bool inherited=false,allow=false;
        for(int i=0;i<count;i++) {
            Need(cursor+8<=size,"ace-header"); byte kind=acl[cursor],flags=acl[cursor+1]; int n=BitConverter.ToUInt16(acl,cursor+2);
            Need((kind==0 || kind==1) && (flags&0xe0)==0 && n>=16 && n%4==0 && cursor+n<=size,"ace-layout");
            string sid=BinarySid(acl,cursor+8,cursor+n);
            Need(n==16+4*acl[cursor+9],"ace-exact-sid");
            bool inh=(flags&0x10)!=0;
            Need(!inherited || inh,"ace-explicit-after-inherited");
            if(inh) inherited=true; else { Need(!allow || kind==0,"explicit-deny-order"); if(kind==0) allow=true; }
            cursor+=n;
        }
        Need(cursor<=size,"acl-used-size");
        return new DaclState { control=(ushort)(control&0x150c),acl=acl };
    }
    public static DaclState Delta(DaclState original,string sid,uint mask,bool station) {
        Need(mask==(station?StationGrant:DesktopGrant),"fixed-grant-mask");
        var identity=new SecurityIdentifier(sid); Need(identity.BinaryLength<=68,"grant-sid-size");
        byte[] bytes=new byte[identity.BinaryLength]; identity.GetBinaryForm(bytes,0);
        int count=BitConverter.ToUInt16(original.acl,4),at=8,insertion=-1;
        Need(count<MaxAces,"ace-count-with-delta");
        for(int i=0;i<count;i++) {
            int n=BitConverter.ToUInt16(original.acl,at+2);
            Need(BinarySid(original.acl,at+8,at+n)!=sid,"preexisting-fresh-sid-ace");
            if(insertion<0 && (original.acl[at+1]&0x10)!=0) insertion=at;
            at+=n;
        }
        if(insertion<0) insertion=at;
        int add=8+bytes.Length; Need(original.acl.Length+add<=65535 && original.acl.Length+add+20<=MaxBytes,"delta-cap");
        byte[] acl=new byte[original.acl.Length+add];
        Array.Copy(original.acl,0,acl,0,insertion);
        // ACCESS_ALLOWED_ACE=0; all AceFlags=0; two DIRECT noninheriting ACEs.
        BitConverter.GetBytes((ushort)add).CopyTo(acl,insertion+2); BitConverter.GetBytes(mask).CopyTo(acl,insertion+4); bytes.CopyTo(acl,insertion+8);
        Array.Copy(original.acl,insertion,acl,insertion+add,original.acl.Length-insertion);
        BitConverter.GetBytes((ushort)acl.Length).CopyTo(acl,2); BitConverter.GetBytes((ushort)(count+1)).CopyTo(acl,4);
        var result=new DaclState { control=original.control,acl=acl };
        ParseDacl(result.Descriptor()); // same structural/canonical reader
        // Exact reverse, including original ACE order and unused bytes.
        byte[] reverse=new byte[original.acl.Length];
        Array.Copy(acl,0,reverse,0,insertion); Array.Copy(acl,insertion+add,reverse,insertion,acl.Length-insertion-add);
        BitConverter.GetBytes((ushort)reverse.Length).CopyTo(reverse,2); BitConverter.GetBytes((ushort)count).CopyTo(reverse,4);
        Need(reverse.SequenceEqual(original.acl),"delta-reverse"); return result;
    }
    public static string RestoreDecision(DaclState current,DaclState original,DaclState expected) {
        if(DaclState.Equal(current,original)) return "original";
        if(DaclState.Equal(current,expected)) return "expected";
        return "foreign";
    }
    public static void InstallDacl(DaclState original,DaclState expected,Func<DaclState> read,Action<DaclState> write,ref bool attempted) {
        Need(expected!=null && DaclState.Equal(read(),original),"pregrant-current-original");
        attempted=true; write(expected);
        Need(DaclState.Equal(read(),expected),"installed-delta-readback");
    }
    public static void RestoreDacl(DaclState original,DaclState expected,bool attempted,Func<DaclState> read,Action<DaclState> write) {
        if(!attempted) { Need(DaclState.Equal(read(),original),"unwritten-object-drift"); return; }
        Need(expected!=null,"expected-state-missing");
        string decision=RestoreDecision(read(),original,expected);
        Need(decision!="foreign","foreign-dacl-retain-vm");
        if(decision=="expected") write(original);
        Need(DaclState.Equal(read(),original),"restore-readback");
    }
    public static int DaclEffectControls(DaclState original,DaclState expected,DaclState foreign) {
        int checks=0,writes=0; bool attempted=false,refused=false; DaclState current=original;
        Action<DaclState> write=s=>{writes++;current=s;};
        InstallDacl(original,expected,()=>current,write,ref attempted);
        Need(attempted && writes==1 && DaclState.Equal(current,expected),"control-install-effect"); checks++;
        RestoreDacl(original,expected,attempted,()=>current,write);
        Need(writes==2 && DaclState.Equal(current,original),"control-restore-effect"); checks++;
        RestoreDacl(original,expected,true,()=>current,write);
        Need(writes==2,"control-D0-no-write"); checks++;
        current=foreign;
        try { RestoreDacl(original,expected,true,()=>current,write); } catch(InvalidOperationException) { refused=true; }
        Need(refused && writes==2 && DaclState.Equal(current,foreign),"control-foreign-zero-writes"); checks++;
        attempted=false; refused=false;
        try { InstallDacl(original,expected,()=>current,write,ref attempted); } catch(InvalidOperationException) { refused=true; }
        Need(refused && !attempted && writes==2,"control-pregrant-drift-no-write"); checks++;
        current=original; attempted=false; refused=false;
        try { InstallDacl(original,expected,()=>current,s=>{writes++;},ref attempted); } catch(InvalidOperationException) { refused=true; }
        Need(refused && attempted && writes==3 && DaclState.Equal(current,original),"control-install-readback"); checks++;
        current=original; attempted=false; refused=false;
        try { InstallDacl(original,expected,()=>current,s=>{writes++;current=s;throw new InvalidOperationException("partial");},ref attempted); }
        catch(InvalidOperationException) { refused=true; }
        Need(refused && attempted && DaclState.Equal(current,expected),"control-partial-grant-owned"); checks++;
        RestoreDacl(original,expected,attempted,()=>current,write);
        Need(DaclState.Equal(current,original),"control-partial-grant-restore"); checks++;
        current=expected; refused=false;
        try { RestoreDacl(original,expected,true,()=>current,s=>{writes++;}); } catch(InvalidOperationException) { refused=true; }
        Need(refused && DaclState.Equal(current,expected),"control-restore-readback"); checks++;
        return checks;
    }
    // Pure bounded reader used by inert controls; never supplies ownership.
    public static void ValidateAccessDescriptor(byte[] check,DaclState expected,string sid) {
        Need(DaclState.Equal(ParseDacl(check),expected),"access-snapshot-dacl");
        uint owner=BitConverter.ToUInt32(check,4),group=BitConverter.ToUInt32(check,8);
        Need(owner>=20 && group>=20 && owner%4==0 && group%4==0,"access-owner-group-present");
        string ownerSid=BinarySid(check,checked((int)owner),check.Length);
        BinarySid(check,checked((int)group),check.Length);
        Need(ownerSid!=sid,"fresh-user-is-object-owner");
    }
    sealed class UserObject : IDisposable {
        IntPtr handle; readonly bool station; public readonly DaclState Original; public DaclState Expected;
        public bool WriteAttempted,Installed,Restored;
        string Name { get { return station?"WinSta0":"Default"; } }
        string Type { get { return station?"WindowStation":"Desktop"; } }
        internal UserObject(bool isStation) {
            station=isStation;
            handle=station?OpenWindowStationW("WinSta0",false,StationOpen):OpenDesktopW("Default",0,false,DesktopOpen);
            if(handle==IntPtr.Zero) throw Error("open-owned-user-object");
            try { Identity(); Original=ParseDacl(Read(4)); } catch { Dispose(); throw; }
        }
        void Identity() { Need(ObjectName(handle,2,Type)==Name,"held-user-object-name"); }
        byte[] Read(uint information) {
            Identity(); byte[] data=new byte[MaxBytes]; uint n;
            if(!GetUserObjectSecurity(handle,ref information,data,MaxBytes,out n)) throw Error("user-security-read");
            Need(n>=20 && n<=MaxBytes,"user-security-size"); Array.Resize(ref data,checked((int)n)); return data;
        }
        void Write(DaclState state) {
            Identity(); uint information=DaclWriteInformation;
            Need(information==4,"dacl-only-write-information");
            if(!SetUserObjectSecurity(handle,ref information,state.Descriptor())) throw Error("user-dacl-write");
            Need(information==4,"dacl-only-write-information-after");
        }
        internal void Plan(string sid) { Expected=Delta(Original,sid,station?StationGrant:DesktopGrant,station); }
        internal void Install() {
            InstallDacl(Original,Expected,()=>ParseDacl(Read(4)),Write,ref WriteAttempted);

            Installed=true;
        }
        internal void Restore() {
            RestoreDacl(Original,Expected,WriteAttempted,()=>ParseDacl(Read(4)),Write); Restored=true;

        }
        internal void CheckB(IntPtr token,string sid) {
            Need(Expected!=null && Installed,"access-before-install");
            // ROOT explicitly extended SOURCE scope: private READ owner/group
            // solely for a valid AccessCheck SD. It NEVER becomes D0 authority.
            byte[] check=Read(7); Need(DaclState.Equal(ParseDacl(check),Expected),"access-snapshot-dacl");
            ValidateAccessDescriptor(check,Expected,sid);
            var mapping=station?new Mapping {read=0x20303,write=0x2001c,execute=0x20060,all=0xf037f}:
                new Mapping {read=0x20041,write=0x200be,execute=0x20100,all=0xf01ff};
            foreach(uint desired in new[]{station?StationGrant:DesktopGrant,0x40000U,0x80000U,0x10000U}) {
                uint n=MaxBytes,granted; bool allowed; byte[] privileges=new byte[MaxBytes];
                if(!AccessCheck(check,token,desired,ref mapping,privileges,ref n,out granted,out allowed)) throw Error("b-access-check");
                Need(n>=8 && n<=MaxBytes,"access-privilege-buffer");
                if(desired==(station?StationGrant:DesktopGrant)) Need(allowed && (granted&desired)==desired,"fixed-candidate-insufficient");
                else Need(!allowed,"b-forbidden-security-authority");
            }
            // ACCESS_SYSTEM_SECURITY is privilege-gated, not a DACL grant.
            // Ordinary() rejects even PRESENT SeSecurityPrivilege; no SACL request.
        }
        public void Dispose() {
            if(handle==IntPtr.Zero) return;
            bool ok=station?CloseWindowStation(handle):CloseDesktop(handle);
            if(!ok) throw Error("owned-user-object-close"); handle=IntPtr.Zero;
        }
        internal string[] Summary() { return new[]{Name,Original.Hash,Expected==null?"":Expected.Hash,Original.Bytes.ToString(),Expected==null?"0":Expected.Bytes.ToString(),Installed.ToString(),Restored.ToString()}; }
    }
    public sealed class Desktops : IDisposable {
        UserObject station,desktop;
        public Desktops() {
            Need(ObjectName(GetProcessWindowStation(),2,"WindowStation")=="WinSta0" &&
                 ObjectName(GetThreadDesktop(GetCurrentThreadId()),2,"Desktop")=="Default","parent-exact-objects");
            try { station=new UserObject(true); desktop=new UserObject(false); } catch { Dispose(); throw; }
        }
        public void Plan(string sid) { station.Plan(sid); desktop.Plan(sid); }
        public void Install() { InstallPair(station.Install,desktop.Install); }
        public void CheckB(Lease owner,string sid) {
            IntPtr token=IntPtr.Zero,imp=IntPtr.Zero;
            try { if(!OpenProcessToken(owner.pi.process,0xa,out token)) throw Error("b-access-token");
                if(!DuplicateTokenEx(token,8,IntPtr.Zero,2,2,out imp)) throw Error("b-access-impersonation-copy");
                station.CheckB(imp,sid); desktop.CheckB(imp,sid);
            } finally { try { KwilOrdinary.Close(ref imp); } finally { KwilOrdinary.Close(ref token); } }
        }
        public void Restore() { desktop.Restore(); station.Restore(); }
        public string[][] Summary() { return new[]{station.Summary(),desktop.Summary()}; }
        public void Dispose() {
            try { if(desktop!=null) desktop.Dispose(); }
            finally { if(station!=null) station.Dispose(); }
        }
    }
}
'@
}

function Copy-OUFixedSource([string]$From,[string]$To,[string]$Hash) {
    Assert-OU ((Get-OUHash $From) -ceq $Hash) 'copy-source-before'
    $inputStream=[IO.File]::OpenRead($From)
    $outputStream=$null
    try {
        Assert-OU ($inputStream.Length -le 1048576) 'copy-source-bound'
        $outputStream=[IO.FileStream]::new($To,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
        $inputStream.CopyTo($outputStream,8192); $outputStream.Flush($true)
    } finally { if($null -ne $outputStream) { $outputStream.Dispose() }; $inputStream.Dispose() }
    Assert-OU ((Get-OUHash $To) -ceq $Hash) 'copy-source-after'
}
function Assert-OUPrivateSource($Spec,[string]$Bootstrap) {
    [KwilOrdinary]::Plain($Spec.root)
    [KwilOrdinary]::Plain($Bootstrap)
    Assert-OU ([IO.Path]::GetFullPath($Bootstrap) -ceq (Join-Path $Spec.root 'source/ci-ordinary-user-bootstrap.ps1')) 'private-source-path'
    Assert-OU ((Get-OUHash $Bootstrap) -ceq $Spec.bootstrapHash -and
        (Get-OUHash (Join-Path $Spec.root 'source/ci-ordinary-user-session.ps1')) -ceq $Spec.sessionHash -and
        (Get-OUHash (Join-Path $Spec.root 'source/ordinary-user-proof.yml')) -ceq $Spec.workflowHash) 'private-source-hashes'
    Assert-OU ($Spec.machine -ceq [Environment]::MachineName) 'private-machine'
}
function Invoke-OUHelper {
    # No secret exists before S owns this H and sends the kernel-peer-bound admission.
    $spec=Read-OUSpec $SpecPath
    Assert-OU ($PipeName -ceq ('kwil-ou-'+$spec.nonce) -and $spec.sid -ceq '') 'helper-invocation'
    Assert-OUHost $spec.architecture
    Assert-OU ($spec.commit -ceq $env:GITHUB_SHA -and $spec.run -ceq $env:GITHUB_RUN_ID) 'helper-run'
    Initialize-OUNative
    Assert-OUPrivateSource $spec $PSCommandPath
    $me=[KwilOrdinary]::Current($true)
    Assert-OUTrustedCallerFacts $me
    [KwilOrdinary]::ServiceReady()
    $pipe=$null; $wire=$null; $credential=$null; $controller=$null; $ok=$false
    try {
        $pipe=[IO.Pipes.NamedPipeClientStream]::new('.',$PipeName,[IO.Pipes.PipeDirection]::InOut,[IO.Pipes.PipeOptions]::Asynchronous)
        $pipe.Connect([Math]::Min(10000,(Get-OURemaining ([long]$spec.supervisorDeadline) 120000)))
        $wire=[KwilOrdinary+Wire]::new($pipe)
        Assert-OU ($wire.Peer($false) -eq [uint32]$spec.supervisorPid) 'helper-server-pid'
        [KwilOrdinary]::VerifyProcess([uint32]$spec.supervisorPid,[long]$spec.supervisorCreated)
        $owned=Read-OUFrame $wire 'owned' $spec @('helperPid','helperCreated','sid','authentication','logon','session','setupDeadline') ([long]$spec.supervisorDeadline-120000)
        Assert-OU ($owned.helperPid -ceq [string]$PID -and $owned.helperCreated -ceq [string][KwilOrdinary]::CurrentCreated()) 'helper-owned-handshake'
        Assert-OU ($owned.sid -ceq $me.UserSid -and $owned.authentication -ceq $me.AuthenticationId -and $owned.logon -ceq $me.LogonSid -and $owned.session -ceq [string]$me.SessionId) 'helper-same-A'
        Assert-OU ($owned.setupDeadline -cmatch '\A[1-9][0-9]{0,18}\z') 'helper-setup-deadline'
        $setupDeadline=[long]$owned.setupDeadline
        [void](Get-OURemaining $setupDeadline)
        # The one mutable pointer is generated INSIDE this now-owned helper.
        $credential=[KwilOrdinary+Credential]::new($spec.user,('Kwil ordinary proof '+$spec.nonce))
        Send-OUFrame $wire 'account' $spec ([ordered]@{sid=$credential.Sid;created='true'}) $setupDeadline
        $grant=Read-OUFrame $wire 'grant-ready' $spec @('sid') $setupDeadline
        Assert-OU ($grant.sid -ceq $credential.Sid) 'helper-granted-sid'
        $sessionSpec=Read-OUSpec (Join-Path $spec.root 'session-spec.json')
        Assert-OU ($sessionSpec.sid -ceq $credential.Sid -and $sessionSpec.nonce -ceq $spec.nonce) 'helper-session-spec'
        $deadline=[Math]::Min([Environment]::TickCount64+90000,$setupDeadline-10000)
        [void](Get-OURemaining $deadline)
        $app=Join-Path $PSHOME 'pwsh.exe'
        [KwilOrdinary]::Plain($app)
        $command=New-OUCommand $app (Join-Path $spec.root 'source/ci-ordinary-user-session.ps1') ('-Role Controller -SpecPath "'+(Join-Path $spec.root 'session-spec.json')+'" -Deadline '+$deadline)
        # This frame marks ambiguity before the potentially blocking broker API.
        Send-OUFrame $wire 'api-entered' $spec ([ordered]@{deadline=[string]$deadline}) $setupDeadline
        try { $controller=$credential.Launch($app,$command,(Join-Path $spec.root 'data'),$spec.machine,$deadline) }
        finally { if($null -eq $controller) { $controller=[KwilOrdinary]::PendingCredential } }
        Assert-OU ($credential.Zeroed -and $credential.Freed) 'helper-secret-cleanup'
        $transfer=$controller.DuplicateTo([uint32]$spec.supervisorPid,[long]$spec.supervisorCreated)
        Send-OUFrame $wire 'handles' $spec ([ordered]@{process=$transfer[0];thread=$transfer[1];pid=$transfer[2];tid=$transfer[3];created=$transfer[4];deadline=$transfer[5];zeroed='true';freed='true'}) $deadline
        $assigned=Read-OUFrame $wire 'assigned' $spec @('pid','created') $deadline
        Assert-OU ($assigned.pid -ceq [string]$controller.Pid -and $assigned.created -ceq [string]$controller.CreatedAt) 'helper-assignment-ack'
        $pending=$controller.Suspended()
        [KwilOrdinary]::Ordinary($pending)
        Assert-OU ($pending.UserSid -ceq $credential.Sid -and $pending.SessionId -eq $me.SessionId) 'helper-B-before-resume'
        $controller.Resume()
        [void]$controller.WaitNatural()
        $cleanup=$controller.Close()
        Assert-OU ($cleanup.Success -and -not $cleanup.DeadlineExpired -and -not $cleanup.TerminationAttempted -and $cleanup.ExitCode -eq 0) 'helper-controller-cleanup'
        Send-OUFrame $wire 'complete' $spec ([ordered]@{pid=[string]$transfer[2];created=[string]$transfer[4];zeroed='true';freed='true';natural='true';closed='true'}) $setupDeadline
        $ok=$true
    } finally {
        if($null -ne $controller) { [void]$controller.Close() }
        if($null -ne $credential) { $credential.Dispose() }
        if($null -ne $wire) { $wire.Dispose() } elseif($null -ne $pipe) { $pipe.Dispose() }
    }
    Assert-OU $ok 'helper-not-complete'
}
function Invoke-OUSupervisor {
    $start=[Environment]::TickCount64; $end=$start+600000; $preflightEnd=$start+60000; $setupEnd=$start+360000
    Assert-OU ($SpecPath -ceq '' -and $PipeName -ceq '') 'supervisor-no-arbitrary-input'
    $arch=$env:KWIL_PROOF_ARCH
    Assert-OUHost $arch
    $repo=Assert-OUSource $PSCommandPath
    Initialize-OUNative
    [void](Get-OURemaining $preflightEnd)
    $a=[KwilOrdinary]::Current($true)
    Assert-OUTrustedCallerFacts $a
    [KwilOrdinary]::ServiceReady()
    $app=Join-Path $PSHOME 'pwsh.exe'; [KwilOrdinary]::Plain($app)
    [KwilOrdinary]::Plain($repo); [KwilOrdinary]::Plain($env:RUNNER_TEMP)
    Assert-OU ([IO.Directory]::Exists($env:RUNNER_TEMP)) 'existing-runner-temp'
    $nonce=[Guid]::NewGuid().ToString('N')
    $root=Join-Path ([IO.Path]::GetFullPath($env:RUNNER_TEMP)) ('kwil-ordinary-'+$nonce)
    $desktops=$null; $job=$null; $helper=$null; $controller=$null; $pipe=$null; $wire=$null
    $planReady=$false; $apiEntered=$false; $knownReturned=$false; $helperComplete=$false; $success=$false
    $restored=$false; $helperStopped=$false; $controllerStopped=$true; $jobEmpty=$false
    $ambiguous=$false; $failure='not-complete'; $sid=''; $bPending=$null; $summary=@()
    $result=[ordered]@{
        schema=1;purpose='OrdinaryUserSessionProof';environmentProof=$false;installerQualification='not-run';fullLifecycleAccepted=$false
        base='26da6914ace64b436ec868f29190cabb541a7b83';commit=$env:GITHUB_SHA;run=$env:GITHUB_RUN_ID;attempt='1';architecture=$arch;nonce=$nonce
        bootstrapHash=$env:KWIL_BOOTSTRAP_SHA256;sessionHash=$env:KWIL_SESSION_SHA256;workflowHash='';approvalHash=(Get-OUTextHash $env:KWIL_OWNER_APPROVAL_REFERENCE)
        candidateRights='UNVERIFIED';accountResidue='no-delete-in-initial-proof';profileResidue='no-explicit-load-unload-or-delete';providerRetirement='trusted-github-hosted-lifecycle-not-a-teardown-receipt'
        launchAmbiguous=$false;daclRestored=$false;ownedJobEmpty=$false;failure=$failure
        fiveLifecycleGaps='all-unwaived';nativeFixKW66='not-run'
    }
    try {
        # Held, typed, noninheritable handles capture D0 before any provisioning.
        $desktops=[KwilOrdinary+Desktops]::new()
        [void](Get-OURemaining $preflightEnd)
        [KwilOrdinary]::FreshDirectory($root)
        Set-OUFreshDirectoryAcl $root $a.UserSid
        $evidenceRoot=Join-Path $env:RUNNER_TEMP 'kwil-ordinary-evidence'
        [KwilOrdinary]::FreshDirectory($evidenceRoot); Set-OUFreshDirectoryAcl $evidenceRoot $a.UserSid
        foreach($child in @('source','data','data/temp')) {
            $path=Join-Path $root $child; [KwilOrdinary]::FreshDirectory($path); Set-OUFreshDirectoryAcl $path $a.UserSid
        }
        $workflow=Join-Path $repo '.github/workflows/ordinary-user-proof.yml'
        $wh=Get-OUHash $workflow; $result.workflowHash=$wh
        Copy-OUFixedSource $PSCommandPath (Join-Path $root 'source/ci-ordinary-user-bootstrap.ps1') $env:KWIL_BOOTSTRAP_SHA256
        Copy-OUFixedSource (Join-Path $repo 'scripts/ci-ordinary-user-session.ps1') (Join-Path $root 'source/ci-ordinary-user-session.ps1') $env:KWIL_SESSION_SHA256
        Copy-OUFixedSource $workflow (Join-Path $root 'source/ordinary-user-proof.yml') $wh
        $spec=[ordered]@{
            schema=1;purpose='OrdinaryUserSessionProof';base=$result.base;commit=$result.commit;run=$result.run;attempt='1';architecture=$arch;nonce=$nonce
            root=$root;machine=[Environment]::MachineName;supervisorPid=[string]$PID;supervisorCreated=[string][KwilOrdinary]::CurrentCreated();supervisorDeadline=[string]$end
            bootstrapHash=$result.bootstrapHash;sessionHash=$result.sessionHash;workflowHash=$wh;approvalHash=$result.approvalHash;user=('kwo'+$nonce.Substring(0,12));sid=''
        }
        Write-OUJson (Join-Path $root 'bootstrap-spec.json') $spec
        $job=[KwilOrdinary+Job]::new()
        $pipeName='kwil-ou-'+$nonce
        $options=[IO.Pipes.PipeOptions]::Asynchronous -bor [IO.Pipes.PipeOptions]::CurrentUserOnly
        $pipe=[IO.Pipes.NamedPipeServerStream]::new($pipeName,[IO.Pipes.PipeDirection]::InOut,1,[IO.Pipes.PipeTransmissionMode]::Byte,$options,4096,4096)
        $command=New-OUCommand $app (Join-Path $root 'source/ci-ordinary-user-bootstrap.ps1') ('-Role Helper -SpecPath "'+(Join-Path $root 'bootstrap-spec.json')+'" -PipeName '+$pipeName)
        $setupEnd=[Math]::Min($start+360000,[Environment]::TickCount64+300000)
        try { $helper=[KwilOrdinary]::SameLaunch($app,$command,$root,$setupEnd,300000) }
        finally { if($null -eq $helper) { $helper=[KwilOrdinary]::PendingSame } }
        $hPending=$helper.Suspended()
        [KwilOrdinary]::Same($a,$hPending)
        $job.Assign($helper)
        $connect=$pipe.WaitForConnectionAsync()
        $helper.Resume()
        Assert-OU ($connect.Wait((Get-OURemaining $setupEnd))) 'helper-connect-deadline'
        $connect.GetAwaiter().GetResult()
        $wire=[KwilOrdinary+Wire]::new($pipe)
        Assert-OU ($wire.Peer($true) -eq $helper.Pid) 'supervisor-helper-peer'
        Send-OUFrame $wire 'owned' $spec ([ordered]@{
            helperPid=[string]$helper.Pid;helperCreated=[string]$helper.CreatedAt;sid=$a.UserSid;authentication=$a.AuthenticationId;logon=$a.LogonSid;session=[string]$a.SessionId;setupDeadline=[string]$setupEnd
        }) $setupEnd
        $account=Read-OUFrame $wire 'account' $spec @('sid','created') $setupEnd
        Assert-OU ($account.created -ceq 'true' -and $account.sid -cmatch '\AS-1-5-21-[0-9]+-[0-9]+-[0-9]+-[1-9][0-9]*\z' -and $account.sid -cne $a.UserSid -and -not $account.sid.EndsWith('-500')) 'fresh-B-sid'
        $sid=$account.sid
        Set-OUFreshDirectoryAcl $root $a.UserSid $sid 0x1200a9
        Set-OUFreshDirectoryAcl (Join-Path $root 'source') $a.UserSid $sid 0x1200a9
        Set-OUFreshDirectoryAcl (Join-Path $root 'data') $a.UserSid $sid 0x1301bf
        Set-OUFreshDirectoryAcl (Join-Path $root 'data/temp') $a.UserSid $sid 0x1301bf
        $spec.sid=$sid
        Write-OUJson (Join-Path $root 'session-spec.json') $spec
        $desktops.Plan($sid); $planReady=$true
        $desktops.Install()
        # Once admission may reach H, a lost frame/return is ambiguous too.
        $apiEntered=$true; $ambiguous=$true; $controllerStopped=$false
        Send-OUFrame $wire 'grant-ready' $spec ([ordered]@{sid=$sid}) $setupEnd
        $entered=Read-OUFrame $wire 'api-entered' $spec @('deadline') $setupEnd
        $apiEntered=$true; $ambiguous=$true; $controllerStopped=$false
        Assert-OU ($entered.deadline -cmatch '\A[1-9][0-9]{0,18}\z') 'controller-deadline-token'
        $cEnd=[long]$entered.deadline
        Assert-OU ($cEnd -le $setupEnd-10000 -and $cEnd -le [Environment]::TickCount64+90000) 'controller-deadline-cap'
        $handles=Read-OUFrame $wire 'handles' $spec @('process','thread','pid','tid','created','deadline','zeroed','freed') $cEnd
        foreach($field in @('process','thread','pid','tid','created','deadline')) { Assert-OU ($handles[$field] -cmatch '\A[1-9][0-9]{0,18}\z') 'transfer-integer' }
        Assert-OU ($handles.deadline -ceq $entered.deadline -and $handles.zeroed -ceq 'true' -and $handles.freed -ceq 'true') 'transfer-correlation'
        # Adopt returned handles BEFORE any other fallible query/receipt/job operation.
        try { $controller=[KwilOrdinary]::AdoptRemote([long]$handles.process,[long]$handles.thread,[uint32]$handles.pid,[uint32]$handles.tid,[long]$handles.created,$cEnd) }
        finally { if($null -eq $controller) { $controller=[KwilOrdinary]::PendingCredential } }
        $knownReturned=$true; $ambiguous=$false
        $bPending=$controller.Suspended(); [KwilOrdinary]::Ordinary($bPending)
        Assert-OU ($bPending.UserSid -ceq $sid -and $bPending.SessionId -eq $a.SessionId -and $bPending.AuthenticationId -cne $a.AuthenticationId -and $bPending.LogonSid -cne $a.LogonSid) 'A-to-B-distinct'
        $job.Assign($controller)
        $desktops.CheckB($controller,$sid)
        Send-OUFrame $wire 'assigned' $spec ([ordered]@{pid=[string]$controller.Pid;created=[string]$controller.CreatedAt}) $cEnd
        $complete=Read-OUFrame $wire 'complete' $spec @('pid','created','zeroed','freed','natural','closed') $setupEnd
        Assert-OU ($complete.pid -ceq $handles.pid -and $complete.created -ceq $handles.created -and $complete.zeroed -ceq 'true' -and $complete.freed -ceq 'true' -and $complete.natural -ceq 'true' -and $complete.closed -ceq 'true') 'helper-completion'
        $helperComplete=$true
        [void]$controller.WaitNatural()
        $cc=$controller.Close()
        $controllerStopped=$cc.Exited
        Assert-OU ($cc.Success -and -not $cc.DeadlineExpired -and -not $cc.TerminationAttempted -and $cc.ExitCode -eq 0) 'supervisor-controller-cleanup'
        [void]$helper.WaitNatural()
        $hc=$helper.Close(); $helperStopped=$hc.Exited
        Assert-OU ($hc.Success -and -not $hc.DeadlineExpired -and -not $hc.TerminationAttempted -and $hc.ExitCode -eq 0) 'supervisor-helper-cleanup'
        $jobEmpty=($job.Active -eq 0)
        Assert-OU $jobEmpty 'owned-extra-activity'
        $cp=Read-OUProof (Join-Path $root 'data/controller.json') $spec 'controller'
        $wp=Read-OUProof (Join-Path $root 'data/worker.json') $spec 'worker'
        Assert-OUActualReceipts $cp $wp $handles ([uint32]$PID)
        Assert-OUFactsJson $bPending $cp.facts -Profile
        Assert-OUFactsJson $bPending $wp.facts -Profile
        Assert-OU ($cp.workerNaturalExit -eq $true -and $cp.workerHandlesClosed -eq $true -and $cp.profileContinuous -eq $true -and $cp.installerQualification -ceq 'not-run' -and $cp.fullLifecycleAccepted -eq $false) 'controller-proof-complete'
        $desktops.Restore(); $restored=$true
        $summary=$desktops.Summary()
        $success=$true; $failure=''
    } catch {
        # Never stringify an exception: it may contain runtime argument text.
        $failure='fixture-failed-'+$_.Exception.GetType().Name
    } finally {
        $cleanupEnd=[Math]::Min($end-20000,[Environment]::TickCount64+120000)
        if(-not $success) {
            if($apiEntered -and -not $knownReturned) { $ambiguous=$true }
            if($null -ne $job) { try { $job.Stop() } catch { $failure+=';job-stop' } }
            if($null -ne $controller) {
                try { $cr=$controller.Close(); $controllerStopped=$cr.Exited; if(-not $cr.Success) { $failure+=';controller-cleanup' } }
                catch { $controllerStopped=$false; $failure+=';controller-close' }
            }
            if($null -ne $helper) {
                try { $hr=$helper.Close(); $helperStopped=$hr.Exited; if(-not $hr.Success) { $failure+=';helper-cleanup' } }
                catch { $helperStopped=$false; $failure+=';helper-close' }
            }
            if($null -ne $job) { try { $jobEmpty=($job.Active -eq 0) } catch { $jobEmpty=$false } }
            # Empty job NEVER resolves an interrupted broker logon.
            if(Test-OUCanRestore $ambiguous $helperStopped $controllerStopped $jobEmpty $planReady) {
                try { [void](Get-OURemaining $cleanupEnd); $desktops.Restore(); $restored=$true }
                catch { $failure+=';restore-retain-vm' }
            }
        }
        if($null -ne $desktops) { try { $summary=$desktops.Summary() } catch { $failure+=';dacl-summary' } }
        if($null -ne $wire) { try { $wire.Dispose() } catch { $failure+=';wire-close' } }
        elseif($null -ne $pipe) { try { $pipe.Dispose() } catch { $failure+=';pipe-close' } }
        if($null -ne $job) { try { $job.Dispose() } catch { $failure+=';job-close'; $success=$false } }
        if($null -ne $desktops) { try { $desktops.Dispose() } catch { $failure+=';desktop-handle-close'; $success=$false } }
    }
    $result.environmentProof=Test-OUEnvironmentSuccess $success $failure $restored $jobEmpty $ambiguous $helperComplete
    $result.launchAmbiguous=$ambiguous; $result.daclRestored=$restored; $result.ownedJobEmpty=$jobEmpty; $result.failure=$failure
    $result.createdSid=$sid; $result.desktopStates=$summary
    $result.parentFacts=Get-OUTokenSummary $a
    if($null -ne $bPending) { $result.suspendedControllerFacts=Get-OUTokenSummary $bPending }
    if($success) { $result.actualControllerFacts=Get-OUTokenSummary $cp.facts; $result.workerFacts=Get-OUTokenSummary $wp.facts; $result.controllerPid=$cp.pid; $result.workerPid=$wp.pid }
    $result.accountResidue=if($sid) { 'exact-created-SID-retained-no-delete' } else { 'create-outcome-possibly-unknown-no-delete' }
    # Public receipts contain no descriptors, credentials, handle values or frames.
    Assert-OU ([IO.Directory]::Exists($evidenceRoot)) 'owned-evidence-root'
    Write-OUJson (Join-Path $evidenceRoot ('environment-'+$arch+'.json')) $result
    Write-Host ('OrdinaryUserSessionProof arch={0} environmentProof={1} launchAmbiguous={2} DACLrestored={3}; installerQualification=not-run; fullLifecycleAccepted=false' -f $arch,$result.environmentProof,$ambiguous,$restored)
    Assert-OU $result.environmentProof 'environment-proof-failed-retain-disposable-vm'
}
if($LibraryOnly) { return }
try {
    if($Role -ceq 'Helper') { Invoke-OUHelper } else { Invoke-OUSupervisor }
} catch {
    # Failure is deliberately non-diagnostic about secrets; no exception/stack dump.
    Write-Host ('OrdinaryUserSessionProof FAILED ({0}); installerQualification=not-run; fullLifecycleAccepted=false; no retry' -f $_.Exception.GetType().Name)
    exit 1
}
exit 0
