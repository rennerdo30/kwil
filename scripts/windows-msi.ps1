#requires -Version 7.2
<#
Schema-2 MSI authoring backend. The six-argument CLI is deliberately parsed
without PowerShell's parameter abbreviation/common-parameter binder. Dot source
without arguments ONLY to load helpers for test-windows-msi.ps1; it performs no
native initialization, filesystem writes or packaging in that mode.

No install/repair/uninstall API is used. ROOT must review and run the authored
fixtures before adoption. The emitted MSI is read back, never rewritten.
#>

function Assert-Kwil {
    param([bool]$Condition, [string]$Reason)
    if (-not $Condition) { throw "Kwil MSI: $Reason" }
}

function Get-KwilReleaseIdentity {
    param([string]$ReleaseVersion)
    if ($ReleaseVersion -ceq '0.1.0') { return [ordered]@{releaseVersion=$ReleaseVersion;channel='stable';msiVersion='0.1.0';productName='Kwil 0.1.0'} }
    Assert-Kwil ($ReleaseVersion -cmatch '\A0[.]1[.]0-beta[.]([1-9][0-9]{0,4})\z') 'noncanonical or unsupported ReleaseVersion'
    $ordinal = [uint32]$Matches[1]
    Assert-Kwil ($ordinal -le 65535) 'ReleaseVersion beta ordinal exceeds MSI range'
    return [ordered]@{releaseVersion=$ReleaseVersion;channel='beta';msiVersion=('0.0.'+$ordinal.ToString([Globalization.CultureInfo]::InvariantCulture));productName=('Kwil Beta '+$ReleaseVersion)}
}

function Get-KwilArguments {
    param([object[]]$Tokens)
    $names = @('PayloadDirectory','OutputPath','ReleaseVersion','Architecture','Commit','IconPath')
    Assert-Kwil ($Tokens.Count -eq 12) 'exactly six full named scalar arguments are required'
    $result = [ordered]@{}
    for ($i = 0; $i -lt 12; $i += 2) {
        $token = $Tokens[$i]
        Assert-Kwil ($token -is [string] -and $token.StartsWith('-')) 'positional argument refused'
        $name = $token.Substring(1)
        Assert-Kwil ($names -ccontains $name) "unknown or abbreviated argument: $token"
        Assert-Kwil (-not $result.Contains($name)) "repeated argument: $name"
        Assert-Kwil ($Tokens[$i+1] -is [string] -and $Tokens[$i+1].Length -gt 0) "non-scalar/empty $name"
        $result.Add($name, $Tokens[$i+1])
    }
    Assert-Kwil ($result.Architecture -cmatch '\A(x64|arm64)\z') 'Architecture must be x64 or arm64'
    Assert-Kwil ($result.Commit -cmatch '\A[0-9a-f]{40}\z') 'Commit must be 40 lowercase hex characters'
    $null = Get-KwilReleaseIdentity $result.ReleaseVersion
    return $result
}

function Get-KwilHash {
    param([byte[]]$Bytes)
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($Bytes)).ToLowerInvariant()
}

function Write-KwilLP {
    param([IO.BinaryWriter]$Writer, [string]$Value)
    $bytes = [Text.UTF8Encoding]::new($false,$true).GetBytes($Value)
    Assert-Kwil ($bytes.Length -le 131068) 'canonical string limit'
    $Writer.Write([uint32]$bytes.Length); $Writer.Write($bytes)
}

function New-KwilEncoding {
    param([string]$Magic, [string[]]$Fields)
    $s = [IO.MemoryStream]::new()
    $w = [IO.BinaryWriter]::new($s,[Text.Encoding]::UTF8,$true)
    try {
        $w.Write([Text.Encoding]::ASCII.GetBytes($Magic))
        foreach ($f in $Fields) { Write-KwilLP $w $f }
        return ,$s.ToArray()
    } finally { $w.Dispose(); $s.Dispose() }
}

function New-KwilUuid5 {
    param([string]$Namespace, [byte[]]$Name)
    $hex = $Namespace.Replace('{','').Replace('}','').Replace('-','')
    $ns = [Convert]::FromHexString($hex)
    $s = [Security.Cryptography.SHA1]::Create()
    try { $hash = $s.ComputeHash([byte[]]($ns + $Name)) } finally { $s.Dispose() }
    $hash[6] = ($hash[6] -band 15) -bor 80
    $hash[8] = ($hash[8] -band 63) -bor 128
    $h = [Convert]::ToHexString($hash[0..15])
    return '{'+$h.Substring(0,8)+'-'+$h.Substring(8,4)+'-'+$h.Substring(12,4)+'-'+$h.Substring(16,4)+'-'+$h.Substring(20,12)+'}'
}

function Get-KwilComponent {
    param([string]$Architecture,[string]$Kind,[string]$Identity)
    $code = New-KwilUuid5 '{E596D49A-3F36-5A1A-89C8-7D052FDE6C47}' (New-KwilEncoding 'KWC1' @($Architecture,$Kind,$Identity,'1'))
    return [pscustomobject]@{ Code=$code; Id='C'+$code.Replace('{','').Replace('}','').Replace('-','') }
}

function New-KwilShortcut {
    $b = [byte[]]::new(868)
    [BitConverter]::GetBytes([uint32]76).CopyTo($b,0)
    [Convert]::FromHexString('0114020000000000C000000000000046').CopyTo($b,4)
    [BitConverter]::GetBytes([uint32]0x02000200).CopyTo($b,20)
    [BitConverter]::GetBytes([uint32]1).CopyTo($b,60)
    [BitConverter]::GetBytes([uint32]788).CopyTo($b,76)
    [BitConverter]::GetBytes([uint32]2684354561).CopyTo($b,80)
    $target = '%LOCALAPPDATA%\Programs\Kwil\kwil.exe'
    [Text.Encoding]::ASCII.GetBytes($target).CopyTo($b,84)
    [Text.Encoding]::Unicode.GetBytes($target).CopyTo($b,344)
    return ,$b
}

function Get-KwilIdentity {
    param($Row)
    if ($Row.kind -ceq 'F') { return $Row.root+':'+$Row.relative.ToLowerInvariant() }
    if ($Row.kind -ceq 'R') { return 'H:'+$Row.relative.ToLowerInvariant()+':'+$Row.name.ToLowerInvariant() }
    Assert-Kwil ($Row.kind -ceq 'K') 'unknown resource kind'
    return 'H:'+$Row.relative.ToLowerInvariant()+':#key'
}

function New-KwilResource {
    param([string]$Kind,[string]$Root,[string]$Relative,[string]$Component)
    return [ordered]@{id='-';kind=$Kind;root=$Root;relative=$Relative;name='-';data='-';size='-';sha256='-';component=$Component;source='-';fileKey='-';registryKey='-';valueType='0'}
}

function New-KwilModel {
    param([string]$Architecture,[System.Collections.IDictionary]$Files)
    $family = '{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}'
    $rows = [Collections.Generic.List[object]]::new()
    $components = @{}
    $expected = @('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update')
    Assert-Kwil ($Files.Count -eq 7) 'schema1 requires exactly seven installed payload files'
    foreach ($name in @($expected + 'Kwil.lnk')) {
        $root = if ($name -ceq 'Kwil.lnk') {'P'} else {'I'}
        $identity = $root+':'+$name.ToLowerInvariant()
        $c = Get-KwilComponent $Architecture 'F' $identity
        $components[$c.Id] = $c.Code
        $f = New-KwilResource 'F' $root $name $c.Id
        $f.source = if ($root -ceq 'P') {'g'} else {'p'}
        if ($root -ceq 'P') { $lnk = New-KwilShortcut; $f.size='868'; $f.sha256=Get-KwilHash $lnk }
        else {
            Assert-Kwil ($Files.Contains($name)) "missing payload file $name"
            $f.size = [string]$Files[$name].Size; $f.sha256 = $Files[$name].Sha256
        }
        $rows.Add($f)
        $r = New-KwilResource 'R' 'H' "Software\Kwil\Installer\$family\Components\$($c.Code)" $c.Id
        $r.name='n:Owner'; $r.data="$family|$Architecture|1|$identity"; $r.valueType='1'
        $rows.Add($r)
    }
    $shell = 'Software\Classes\*\shell\Kwil.3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A'
    foreach ($s in @(@($shell,'@','Open with Kwil'),@($shell,'n:Icon','"${I}kwil.exe",0'),@(($shell+'\command'),'@','"${I}kwil.exe" "%1"'))) {
        $r = New-KwilResource 'R' 'H' $s[0] '-'
        $r.name=$s[1]; $r.data=$s[2]; $r.valueType='1'
        $c = Get-KwilComponent $Architecture 'R' (Get-KwilIdentity $r)
        $r.component=$c.Id; $components[$c.Id]=$c.Code; $rows.Add($r)
    }
    $sorted = [Collections.Generic.SortedDictionary[string,object]]::new([StringComparer]::Ordinal)
    foreach ($r in $rows) { $sorted.Add((Get-KwilIdentity $r),$r) }
    $keys = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($r in @($sorted.Values)) {
        if ($r.kind -ceq 'R' -and $keys.Add($r.relative)) {
            $k=New-KwilResource 'K' 'H' $r.relative $r.component
            $sorted.Add((Get-KwilIdentity $k),$k)
        }
    }
    $i=0
    foreach ($r in $sorted.Values) {
        $i++; $digits=$i.ToString('D6',[Globalization.CultureInfo]::InvariantCulture)
        $r.id='r'+$digits
        if ($r.kind -ceq 'F') { $r.fileKey='F'+$digits }
        if ($r.kind -ceq 'R') { $r.registryKey='R'+$digits }
    }
    Assert-Kwil ($i -eq 29 -and $components.Count -eq 11 -and $keys.Count -eq 10) 'schema1 resource counts'
    return [pscustomobject]@{Rows=@($sorted.Values); Components=$components}
}

function Get-KwilInventoryBytes {
    param([object[]]$Rows)
    Assert-Kwil ($Rows.Count -le 512) 'resource count limit'
    $s=[IO.MemoryStream]::new(); $w=[IO.BinaryWriter]::new($s,[Text.Encoding]::UTF8,$true)
    try {
        $w.Write([Text.Encoding]::ASCII.GetBytes('KWR1')); $w.Write([uint32]$Rows.Count)
        foreach ($r in $Rows) {
            foreach ($field in @('id','kind','root','relative','name','data','size','sha256','component','source','fileKey','registryKey','valueType')) { Write-KwilLP $w $r[$field] }
            Assert-Kwil ($s.Length -le 1048576) 'inventory byte limit'
        }
        return ,$s.ToArray()
    } finally { $w.Dispose(); $s.Dispose() }
}

function Get-KwilBuildId {
    param($Metadata,[byte[]]$Inventory)
    $release = Get-KwilReleaseIdentity $Metadata.releaseVersion
    Assert-Kwil ($Metadata.channel -ceq $release.channel -and $Metadata.msiVersion -ceq $release.msiVersion) 'release/channel/MSI mapping mismatch'
    $s=[IO.MemoryStream]::new(); $w=[IO.BinaryWriter]::new($s,[Text.Encoding]::UTF8,$true)
    try {
        $w.Write([Text.Encoding]::ASCII.GetBytes('KWB2'))
        foreach ($v in @('2','1',$Metadata.familyGuid,$Metadata.architecture,$Metadata.releaseVersion,$Metadata.channel,$Metadata.msiVersion,$Metadata.commit,$Metadata.target,'2',$Metadata.guard.sha256)) { Write-KwilLP $w $v }
        $w.Write([uint32]$Metadata.guard.imports.Count)
        foreach ($im in $Metadata.guard.imports) { Write-KwilLP $w $im.load; Write-KwilLP $w $im.dll }
        $w.Write([uint32]$Inventory.Length); $w.Write($Inventory)
        return Get-KwilHash $s.ToArray()
    } finally { $w.Dispose(); $s.Dispose() }
}

function ConvertFrom-KwilJsonElement {
    param([Text.Json.JsonElement]$Element,[int]$Depth=0)
    Assert-Kwil ($Depth -le 8) 'JSON depth limit'
    switch ($Element.ValueKind.ToString()) {
        Object {
            # JsonDocument is a lossless syntax tree (it retains duplicate names).
            # Check decoded names before constructing the semantic object/map.
            $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            foreach ($p in $Element.EnumerateObject()) {
                Assert-Kwil ($seen.Add($p.Name)) "duplicate JSON member $($p.Name)"
                Assert-Kwil ($seen.Count -le 512) 'JSON object member count limit'
            }
            $r=[ordered]@{}
            foreach ($p in $Element.EnumerateObject()) {
                $r.Add($p.Name,(ConvertFrom-KwilJsonElement $p.Value ($Depth+1)))
            }
            return ,$r
        }
        Array {
            Assert-Kwil ($Element.GetArrayLength() -le 512) 'JSON array count limit'
            $r=[Collections.Generic.List[object]]::new()
            foreach ($v in $Element.EnumerateArray()) { $r.Add((ConvertFrom-KwilJsonElement $v ($Depth+1))) }
            return ,$r.ToArray()
        }
        String {
            $v=$Element.GetString()
            Assert-Kwil ($v.Length -le 32767 -and $v.IndexOf([char]0) -lt 0) 'JSON string length/NUL'
            for ($i=0;$i -lt $v.Length;$i++) {
                if ([char]::IsHighSurrogate($v[$i])) { Assert-Kwil ($i+1 -lt $v.Length -and [char]::IsLowSurrogate($v[$i+1])) 'unpaired surrogate'; $i++ }
                else { Assert-Kwil (-not [char]::IsLowSurrogate($v[$i])) 'unpaired surrogate' }
            }
            return $v
        }
        Number { Assert-Kwil ($Element.GetRawText() -cmatch '^(0|[1-9][0-9]{0,9})$') 'JSON requires unsigned integer'; return $Element.GetInt64() }
        default { throw 'Kwil MSI: unexpected JSON primitive' }
    }
}

function Read-KwilJson {
    param([byte[]]$Bytes)
    Assert-Kwil ($Bytes.Length -le 1048576 -and $Bytes.Length -gt 0) 'input JSON byte limit'
    Assert-Kwil (-not ($Bytes.Length -ge 3 -and $Bytes[0] -eq 239 -and $Bytes[1] -eq 187 -and $Bytes[2] -eq 191)) 'JSON BOM refused'
    $utf8=[Text.UTF8Encoding]::new($false,$true)
    $text=$utf8.GetString($Bytes)
    $options=[Text.Json.JsonDocumentOptions]::new(); $options.MaxDepth=8
    $doc=[Text.Json.JsonDocument]::Parse($text,$options)
    try { return ConvertFrom-KwilJsonElement $doc.RootElement } finally { $doc.Dispose() }
}

function Assert-KwilMembers {
    param($Object,[string[]]$Names)
    Assert-Kwil ($Object -is [Collections.IDictionary] -and $Object.Count -eq $Names.Count) 'JSON object shape'
    foreach ($key in $Object.Keys) { Assert-Kwil ($Names -ccontains $key) "unknown JSON member $key" }
}

function Assert-KwilMetadata {
    param($Metadata,$Arguments,$Model,[string]$GuardHash,[object[]]$Imports)
    Assert-KwilMembers $Metadata @('schema','componentSchema','familyGuid','releaseVersion','channel','msiVersion','commit','architecture','target','guard','inventory')
    Assert-Kwil ($Metadata.schema -is [long] -and $Metadata.schema -eq 2) 'invalid schema'
    Assert-Kwil ($Metadata.componentSchema -is [long] -and $Metadata.componentSchema -eq 1) 'invalid componentSchema'
    $release = Get-KwilReleaseIdentity $Arguments.ReleaseVersion
    $fixed=@{familyGuid='{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}';releaseVersion=$Arguments.ReleaseVersion;channel=$release.channel;msiVersion=$release.msiVersion;commit=$Arguments.Commit;architecture=$Arguments.Architecture;target=$(if($Arguments.Architecture -ceq 'x64'){'x86_64-pc-windows-msvc'}else{'aarch64-pc-windows-msvc'})}
    foreach ($k in $fixed.Keys) { Assert-Kwil ($Metadata[$k] -is [string] -and $Metadata[$k] -ceq $fixed[$k]) "metadata $k mismatch" }
    Assert-KwilMembers $Metadata.guard @('abi','sha256','imports')
    Assert-Kwil ($Metadata.guard.abi -is [long] -and $Metadata.guard.abi -eq 2 -and $Metadata.guard.sha256 -is [string] -and $Metadata.guard.sha256 -ceq $GuardHash) 'guard ABI/hash mismatch'
    Assert-Kwil ($Metadata.guard.imports -is [object[]] -and $Metadata.guard.imports.Count -eq $Imports.Count) 'guard import count mismatch'
    for ($i=0;$i -lt $Imports.Count;$i++) {
        $r=$Metadata.guard.imports[$i]; Assert-KwilMembers $r @('load','dll')
        Assert-Kwil ($r.load -is [string] -and $r.dll -is [string] -and $r.load -ceq $Imports[$i].load -and $r.dll -ceq $Imports[$i].dll) 'guard normal/delay import mismatch'
    }
    Assert-Kwil ($Metadata.inventory -is [object[]] -and $Metadata.inventory.Count -eq 29) 'inventory must contain exactly 29 rows'
    for ($i=0;$i -lt 29;$i++) {
        $row=$Metadata.inventory[$i]; $expected=$Model.Rows[$i]
        Assert-KwilMembers $row @($expected.Keys)
        foreach ($k in $expected.Keys) { Assert-Kwil ($row[$k] -is [string] -and $row[$k] -ceq $expected[$k]) "inventory mismatch row $i field $k" }
    }
}

function Initialize-KwilNative {
    if ('KwilMsi.Native' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Text;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
using System.IO;
using System.Threading.Tasks;
namespace KwilMsi {
 [StructLayout(LayoutKind.Sequential)] public struct FileInfo {
  public uint Attributes; public System.Runtime.InteropServices.ComTypes.FILETIME Created,Accessed,Written;
  public uint Volume,SizeHigh,SizeLow,Links,IndexHigh,IndexLow;
 }
 public static class Native {
  public static async Task<bool> Drain(Stream source,Stream destination) {
   byte[] buffer=new byte[4096]; int n,total=0; bool bounded=true;
   while((n=await source.ReadAsync(buffer,0,buffer.Length))!=0) {
    if(total<=1048576-n) {await destination.WriteAsync(buffer,0,n);total+=n;}
    else {bounded=false;}
   }
   return bounded;
  }
  [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] public static extern SafeFileHandle CreateFileW(string p,uint a,uint s,IntPtr sec,uint c,uint f,IntPtr t);
  [DllImport("kernel32.dll",SetLastError=true)] public static extern bool GetFileInformationByHandle(SafeFileHandle h,out FileInfo i);
  [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] public static extern uint GetFinalPathNameByHandleW(SafeFileHandle h,StringBuilder b,uint n,uint flags);
  [DllImport("msi.dll",CharSet=CharSet.Unicode)] public static extern uint MsiOpenDatabaseW(string p,IntPtr mode,out uint db);
  [DllImport("msi.dll",CharSet=CharSet.Unicode)] public static extern uint MsiDatabaseOpenViewW(uint db,string q,out uint view);
  [DllImport("msi.dll")] public static extern uint MsiViewExecute(uint view,uint record);
  [DllImport("msi.dll")] public static extern uint MsiViewFetch(uint view,out uint record);
  [DllImport("msi.dll")] public static extern uint MsiRecordReadStream(uint record,uint field,byte[] b,ref uint n);
  [DllImport("msi.dll")] public static extern uint MsiCloseHandle(uint h);
 }
}
'@
}

function Get-KwilHandleInfo {
    param($Handle)
    $info=[KwilMsi.FileInfo]::new()
    Assert-Kwil ([KwilMsi.Native]::GetFileInformationByHandle($Handle,[ref]$info)) 'file identity query failed'
    Assert-Kwil (($info.Attributes -band 0x400) -eq 0) 'reparse point refused'
    return $info
}

function Get-KwilFinalPath {
    param($Handle)
    $b=[Text.StringBuilder]::new(32767)
    $n=[KwilMsi.Native]::GetFinalPathNameByHandleW($Handle,$b,32767,0)
    Assert-Kwil ($n -gt 0 -and $n -lt 32767) 'canonical path query failed or exceeded limit'
    return $b.ToString().TrimEnd('\')
}

function Assert-KwilLocalPath {
    param([string]$Path,[bool]$Absent=$false,[bool]$Directory=$false)
    Assert-Kwil ($Path.Length -lt 32767 -and $Path -cmatch '^[A-Za-z]:[\\/]' -and $Path -notmatch '[\x00-\x1f"]') 'absolute local safe path required'
    Assert-Kwil ($Path.Substring(2) -notmatch ':' -and $Path -notmatch '[*?]') 'device/ADS/wildcard path refused'
    $full=[IO.Path]::GetFullPath($Path).TrimEnd('\','/')
    $parts=$full.Substring(3).Split('\')
    foreach ($p in $Path.Substring(3).Split([char[]]@('\','/'))) { Assert-Kwil ($p -ne '.' -and $p -ne '..' -and $p -notmatch '[. ]$') 'noncanonical path segment' }
    $current=$full.Substring(0,3); $previous=$null
    $segments=@('')+@($parts | Where-Object { $_.Length -gt 0 })
    for($i=0;$i -lt $segments.Count;$i++) {
        if($i -gt 0) { $current=[IO.Path]::Combine($current,$segments[$i]) }
        $last=$i -eq $segments.Count-1
        $h=[KwilMsi.Native]::CreateFileW($current,0x80,1,[IntPtr]::Zero,3,0x02200000,[IntPtr]::Zero)
        try {
            if($h.IsInvalid) {
                $e=[Runtime.InteropServices.Marshal]::GetLastWin32Error()
                Assert-Kwil ($last -and $Absent -and $e -eq 2) "path open failed ($e): $current"
                return $full
            }
            Assert-Kwil (-not ($last -and $Absent)) "output already exists: $current"
            $info=Get-KwilHandleInfo $h
            Assert-Kwil (($last -and -not $Directory) -or ($info.Attributes -band 0x10) -ne 0) 'path ancestor is not a directory'
            $actual=Get-KwilFinalPath $h
            if($null -ne $previous) { Assert-Kwil ([StringComparer]::OrdinalIgnoreCase.Equals($actual,$previous+'\'+$segments[$i])) 'filesystem alias/short-name/redirection refused' }
            $previous=$actual
        } finally { $h.Dispose() }
    }
    return $full
}

function Open-KwilInput {
    param([string]$Path,[long]$Maximum)
    $null=Assert-KwilLocalPath $Path
    $h=[KwilMsi.Native]::CreateFileW($Path,2147483776,1,[IntPtr]::Zero,3,0x00200000,[IntPtr]::Zero)
    Assert-Kwil (-not $h.IsInvalid) "read-only input open failed: $Path"
    try {
        $info=Get-KwilHandleInfo $h
        Assert-Kwil (($info.Attributes -band 0x10) -eq 0 -and $info.Links -eq 1) 'input must be a regular single-link file'
        $size=([long]$info.SizeHigh -shl 32)+$info.SizeLow
        Assert-Kwil ($size -le $Maximum) 'input file size limit'
        $s=[IO.FileStream]::new($h,[IO.FileAccess]::Read,65536,$false)
        $sha=[Security.Cryptography.SHA256]::Create()
        try { $hash=[Convert]::ToHexString($sha.ComputeHash($s)).ToLowerInvariant() } finally { $sha.Dispose() }
        $s.Position=0
        return [pscustomobject]@{Path=$Path;Stream=$s;Info=$info;Size=$size;Sha256=$hash;Final=(Get-KwilFinalPath $h)}
    } catch { $h.Dispose(); throw }
}

function Assert-KwilInputStable {
    param($InputFile)
    $after=Get-KwilHandleInfo $InputFile.Stream.SafeFileHandle
    foreach($k in @('Volume','IndexHigh','IndexLow','SizeHigh','SizeLow','Links')) { Assert-Kwil ($after.$k -eq $InputFile.Info.$k) 'input identity/length changed' }
    Assert-Kwil ($after.Written.dwHighDateTime -eq $InputFile.Info.Written.dwHighDateTime -and $after.Written.dwLowDateTime -eq $InputFile.Info.Written.dwLowDateTime) 'input timestamp changed'
    $InputFile.Stream.Position=0; $sha=[Security.Cryptography.SHA256]::Create()
    try { $h=[Convert]::ToHexString($sha.ComputeHash($InputFile.Stream)).ToLowerInvariant() } finally { $sha.Dispose() }
    Assert-Kwil ($h -ceq $InputFile.Sha256) 'immutable input hash changed'
}

function Read-KwilAt {
    param([IO.Stream]$Stream,[long]$Offset,[int]$Count)
    Assert-Kwil ($Offset -ge 0 -and $Count -ge 0 -and $Count -le 65536 -and $Offset -le $Stream.Length-$Count) 'binary offset/count out of bounds'
    $Stream.Position=$Offset; $b=[byte[]]::new($Count); $done=0
    while($done -lt $Count) { $n=$Stream.Read($b,$done,$Count-$done); Assert-Kwil ($n -gt 0) 'unexpected binary EOF'; $done+=$n }
    return ,$b
}

function Get-KwilPeImports {
    param([IO.Stream]$Stream,[string]$Architecture,[string]$Kind)
    $dos=Read-KwilAt $Stream 0 64
    Assert-Kwil ($dos[0] -eq 77 -and $dos[1] -eq 90) 'not a PE image'
    $pe=[BitConverter]::ToUInt32($dos,60); $h=Read-KwilAt $Stream $pe 24
    Assert-Kwil ([BitConverter]::ToUInt32($h,0) -eq 0x4550) 'invalid PE signature'
    $machine=if($Architecture -ceq 'x64'){0x8664}else{0xaa64}
    Assert-Kwil ([BitConverter]::ToUInt16($h,4) -eq $machine) 'PE machine mismatch'
    $ns=[BitConverter]::ToUInt16($h,6); $no=[BitConverter]::ToUInt16($h,20)
    Assert-Kwil ($ns -ge 1 -and $ns -le 96 -and $no -ge 240 -and $no -le 4096) 'PE header count/size limit'
    $o=Read-KwilAt $Stream ($pe+24) $no
    Assert-Kwil ([BitConverter]::ToUInt16($o,0) -eq 0x20b -and [BitConverter]::ToUInt32($o,108) -ge 14) 'PE32+ normal/delay directories required'
    $base=[BitConverter]::ToUInt64($o,24)
    $sec=Read-KwilAt $Stream ($pe+24+$no) ($ns*40)
    $sections=[Collections.Generic.List[object]]::new()
    for($i=0;$i -lt $ns;$i++) {
        $k=$i*40
        $s=[pscustomobject]@{VA=[long][BitConverter]::ToUInt32($sec,$k+12);Size=[long][Math]::Min([BitConverter]::ToUInt32($sec,$k+8),[BitConverter]::ToUInt32($sec,$k+16));Raw=[long][BitConverter]::ToUInt32($sec,$k+20)}
        Assert-Kwil ($s.Raw+$s.Size -le $Stream.Length) 'PE section outside file'
        foreach($prev in $sections) { Assert-Kwil ($s.VA+$s.Size -le $prev.VA -or $prev.VA+$prev.Size -le $s.VA) 'overlapping PE sections' }
        $sections.Add($s)
    }
    $rvaRead = {
        param([long]$Rva,[int]$Count)
        foreach($s in $sections) { if($Rva -ge $s.VA -and $Rva+$Count -le $s.VA+$s.Size) { return ,(Read-KwilAt $Stream ($s.Raw+$Rva-$s.VA) $Count) } }
        throw 'Kwil MSI: PE RVA outside backed section'
    }
    $readName = {
        param([long]$Rva,[int]$Max)
        $chars=[Collections.Generic.List[byte]]::new()
        for($j=0;$j -le $Max;$j++) {
            $v=(& $rvaRead ($Rva+$j) 1)[0]
            if($v -eq 0) { return [Text.Encoding]::ASCII.GetString($chars.ToArray()) }
            Assert-Kwil ($j -lt $Max -and $v -ge 33 -and $v -le 126) 'PE name invalid or unterminated'; $chars.Add($v)
        }
    }
    $imports=[Collections.Generic.SortedDictionary[string,object]]::new([StringComparer]::Ordinal)
    $descriptorCount=0
    foreach($d in @(@(1,20,'N'),@(13,32,'D'))) {
        $rva=[long][BitConverter]::ToUInt32($o,112+$d[0]*8); $size=[long][BitConverter]::ToUInt32($o,116+$d[0]*8)
        if($rva -eq 0 -and $size -eq 0) { continue }
        Assert-Kwil ($rva -gt 0 -and $size -ge $d[1] -and $size -le 129*$d[1]) 'PE import directory limit'
        $terminated=$false
        for($p=0;$p+$d[1] -le $size;$p+=$d[1]) {
            $entry=& $rvaRead ($rva+$p) $d[1]
            $allzero=$true; foreach($v in $entry) { if($v -ne 0){$allzero=$false;break} }
            if($allzero){$terminated=$true;break}
            Assert-Kwil ($descriptorCount -lt 128) 'too many physical imports'
            $descriptorCount++
            $nr=[long][BitConverter]::ToUInt32($entry,$(if($d[2] -ceq 'N'){12}else{4}))
            if($d[2] -ceq 'D') {
                $attrs=[BitConverter]::ToUInt32($entry,0); Assert-Kwil (($attrs -band 4294967294L) -eq 0) 'unsupported delay import attributes'
                if(($attrs -band 1) -eq 0) { Assert-Kwil ([uint64]$nr -ge $base) 'invalid delay VA'; $nr=[long]([uint64]$nr-$base) }
            }
            $dll=(& $readName $nr 128).ToLowerInvariant()
            Assert-Kwil ($dll -cmatch '^[a-z0-9._-]+[.]dll$') 'invalid DLL basename'
            Assert-Kwil ($dll -notmatch '^(vcruntime|msvcp|msvcr|ucrtbase|api-ms-win-crt-)') "dynamic runtime import $dll"
            # Validate every physical entry before emitting the canonical ledger set.
            $key=$d[2]+':'+$dll
            if(-not $imports.ContainsKey($key)) { $imports.Add($key,[ordered]@{load=$d[2];dll=$dll}) }
        }
        Assert-Kwil $terminated 'unterminated PE import directory'
    }
    Assert-Kwil ($imports.Count -gt 0) 'expected system imports'
    $isDll=([BitConverter]::ToUInt16($h,22) -band 0x2000) -ne 0
    if($Kind -ceq 'guard') {
        Assert-Kwil $isDll 'guard must be a DLL'
        $er=[long][BitConverter]::ToUInt32($o,112); $es=[long][BitConverter]::ToUInt32($o,116)
        Assert-Kwil ($er -gt 0 -and $es -ge 40) 'guard export directory missing'
        $e=& $rvaRead $er 40; $nf=[BitConverter]::ToUInt32($e,20); $nn=[BitConverter]::ToUInt32($e,24)
        Assert-Kwil ($nf -gt 0 -and $nf -le 128 -and $nn -gt 0 -and $nn -le 128) 'guard export count limit'
        Assert-Kwil ($nf -eq 1 -and $nn -eq 1) 'schema-2 guard requires the sole ABI2 export'
        $found=0
        for($i=0;$i -lt $nn;$i++) {
            $ptr=& $rvaRead ([long][BitConverter]::ToUInt32($e,32)+4*$i) 4
            if((& $readName ([BitConverter]::ToUInt32($ptr,0)) 128) -ceq 'KwilPreflightV2') {
                $ord=& $rvaRead ([long][BitConverter]::ToUInt32($e,36)+2*$i) 2; $index=[BitConverter]::ToUInt16($ord,0)
                Assert-Kwil ($index -lt $nf) 'guard export ordinal invalid'
                $ptr=& $rvaRead ([long][BitConverter]::ToUInt32($e,28)+4*$index) 4; $fn=[long][BitConverter]::ToUInt32($ptr,0)
                Assert-Kwil ($fn -gt 0 -and ($fn -lt $er -or $fn -ge $er+$es)) 'guard export is forwarded/null'
                $null=& $rvaRead $fn 1; $found++
            }
        }
        Assert-Kwil ($found -eq 1) 'required unique guard ABI export missing'
    } else {
        Assert-Kwil (-not $isDll) 'entry point must not be a DLL'
        Assert-Kwil ([BitConverter]::ToUInt16($o,68) -eq $(if($Kind -ceq 'exe'){2}else{3})) 'entry-point subsystem mismatch'
    }
    return ,@($imports.Values)
}

function Get-KwilTableSpecs {
    # Uppercase initial means nullable. No localization flags are implicit.
    $s=[ordered]@{
        Property=@('Property:s72','Value:S0')
        Directory=@('Directory:s72','Directory_Parent:S72','DefaultDir:s255')
        Component=@('Component:s72','ComponentId:S38','Directory_:s72','Attributes:i2','Condition:S255','KeyPath:S72')
        Feature=@('Feature:s38','Feature_Parent:S38','Title:S64','Description:S255','Display:I2','Level:i2','Directory_:S72','Attributes:i2')
        FeatureComponents=@('Feature_:s38','Component_:s72')
        File=@('File:s72','Component_:s72','FileName:s255','FileSize:i4','Version:S72','Language:S20','Attributes:I2','Sequence:i4')
        Registry=@('Registry:s72','Root:i2','Key:s255','Name:S255','Value:S0','Component_:s72')
        RemoveFile=@('FileKey:s72','Component_:s72','FileName:S255','DirProperty:s72','InstallMode:i2')
        Media=@('DiskId:i2','LastSequence:i4','DiskPrompt:S64','Cabinet:S255','VolumeLabel:S32','Source:S72')
        Binary=@('Name:s72','Data:v0')
        Icon=@('Name:s72','Data:v0')
        CustomAction=@('Action:s72','Type:i2','Source:S72','Target:S255')
        InstallExecuteSequence=@('Action:s72','Condition:S255','Sequence:I2')
        Upgrade=@('UpgradeCode:s38','VersionMin:S20','VersionMax:S20','Language:S255','Attributes:i4','Remove:S255','ActionProperty:s72')
        Error=@('Error:i2','Message:S0')
        LaunchCondition=@('Condition:s255','Description:s255')
        KwilPackage=@('Id:i2','Schema:i2','ComponentSchema:i2','Family:s38','Architecture:s5','ReleaseVersion:s18','Channel:s6','MsiVersion:s20','Commit:s40','Target:s32','GuardAbi:i2','GuardSha256:s64','InventorySha256:s64','BuildId:s64','ProductCode:s38','PackageCode:s38','ResourceCount:i4')
        KwilResource=@('Id:s32','Kind:s1','Root:s1','Relative:s240','Name:s130','Data:s0','Size:s20','Sha256:s64','Component_:s72','Source:s1','File_:s72','Registry_:s72','ValueType:i2')
        KwilGuardImport=@('Load:s1','Dll:s128')
        _Validation=@('Table:s32','Column:s32','Nullable:s4','MinValue:I4','MaxValue:I4','KeyTable:S255','KeyColumn:I2','Category:S32','Set:S255','Description:S255')
    }
    $result=[ordered]@{}
    foreach($name in $s.Keys) {
        $cols=@(foreach($c in $s[$name]) { $p=$c.Split(':'); [pscustomobject]@{Name=$p[0];Type=$p[1]} })
        $keys=switch($name){FeatureComponents{@('Feature_','Component_')};Upgrade{@('UpgradeCode','VersionMin','VersionMax','Language','Attributes')};KwilGuardImport{@('Load','Dll')};_Validation{@('Table','Column')};default{@($cols[0].Name)}}
        $result[$name]=[pscustomobject]@{Columns=$cols;Keys=@($keys);Rows=[Collections.Generic.List[object]]::new()}
    }
    return $result
}

function Add-KwilRow {
    param($Tables,[string]$Table,[object[]]$Values)
    Assert-Kwil ($Values.Count -eq $Tables[$Table].Columns.Count) "authoring column count $Table"
    $Tables[$Table].Rows.Add($Values)
}

function Get-KwilShortName {
    param($Row)
    Assert-Kwil ($Row.relative -cmatch '^[A-Za-z0-9._-]+$' -and $Row.relative -notmatch '[. ]$' -and $Row.relative -notmatch '^(?i:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)') 'unsafe file spelling'
    $ext=[IO.Path]::GetExtension($Row.relative).TrimStart('.').ToLowerInvariant() -replace '[^a-z0-9]',''
    if($ext.Length -gt 3){$ext=$ext.Substring(0,3)}
    $short='K'+$Row.id.Substring(1)
    if($ext.Length -gt 0){$short+='.'+$ext}
    return $short+'|'+$Row.relative
}

function New-KwilTables {
    param($Metadata,$Model,[string]$BuildId,[string]$InventoryHash,[string]$ProductCode,[string]$PackageCode,[string]$GuardPath,[string]$IconPath)
    $t=Get-KwilTableSpecs
    $release=Get-KwilReleaseIdentity $Metadata.releaseVersion
    Assert-Kwil ($Metadata.channel -ceq $release.channel -and $Metadata.msiVersion -ceq $release.msiVersion) 'release/channel/MSI mapping mismatch'
    $props=[ordered]@{ProductName=$release.productName;Manufacturer='rennerdo30';ProductLanguage='1033';UpgradeCode=$Metadata.familyGuid;ProductCode=$ProductCode;ProductVersion=$Metadata.msiVersion;MSIINSTALLPERUSER='1';ARPNOMODIFY='1';ARPNOREPAIR='1';ARPPRODUCTICON='KwilIcon';ARPINSTALLLOCATION='[INSTALLDIR]';MSIRESTARTMANAGERCONTROL='Disable';REBOOT='ReallySuppress';KWIL_SCHEMA='2';KWIL_COMPONENT_SCHEMA='1';KWIL_GUARD_ABI='2';KWIL_RELEASE_VERSION=$Metadata.releaseVersion;KWIL_CHANNEL=$Metadata.channel;KWIL_FAMILY=$Metadata.familyGuid;KWIL_ARCH=$Metadata.architecture;KWIL_BUILD_ID=$BuildId;SecureCustomProperties='KWIL_OLDER;KWIL_EQUAL;KWIL_NEWER'}
    foreach($p in $props.Keys){Add-KwilRow $t Property @($p,$props[$p])}
    Add-KwilRow $t Directory @('TARGETDIR',$null,'SourceDir')
    Add-KwilRow $t Directory @('LocalAppDataFolder','TARGETDIR','.')
    Add-KwilRow $t Directory @('INSTALLPROGRAMS','LocalAppDataFolder','PROGRA~1|Programs')
    Add-KwilRow $t Directory @('INSTALLDIR','INSTALLPROGRAMS','Kwil')
    Add-KwilRow $t Directory @('ProgramMenuFolder','TARGETDIR','.')
    Add-KwilRow $t Feature @('Main',$null,'Kwil','Kwil',1,1,'INSTALLDIR',0)
    $keypaths=@{}; $directories=@{}; $sequence=0
    $spellings=[Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach($r in $Model.Rows){if($r.kind -ceq 'F'){Assert-Kwil ($spellings.Add($r.relative)) 'long filename collision'}}
    foreach($r in $Model.Rows) {
        $values=@(foreach($k in $r.Keys){if($k -ceq 'valueType'){[int]$r[$k]}else{$r[$k]}})
        Add-KwilRow $t KwilResource $values
        if($r.kind -ceq 'F') {
            $sequence++; $dir=if($r.root -ceq 'I'){'INSTALLDIR'}else{'ProgramMenuFolder'}
            $directories[$r.component]=$dir
            $name=Get-KwilShortName $r
            Assert-Kwil ($spellings.Add($name.Split('|')[0])) 'short filename collision'
            Add-KwilRow $t File @($r.fileKey,$r.component,$name,[int]$r.size,$null,$null,16384,$sequence)
        }
        if($r.kind -ceq 'R') {
            $name=if($r.name -ceq '@'){$null}else{$r.name.Substring(2)}
            if($r.name -ceq 'n:Owner') {
                # The only literal-brace exception is this exact file marker.
                # A family-shaped prefix alone cannot authorize other data.
                $ownedFile=@($Model.Rows | Where-Object{$_.kind -ceq 'F' -and $_.component -ceq $r.component})
                Assert-Kwil ($ownedFile.Count -eq 1) 'Owner must map to exactly one file'
                $family='{3D9C2C6F-7C59-4D54-92B4-0BA1EC60347A}'
                $ownerLiteral=$family+'|'+$Metadata.architecture+'|1|'+(Get-KwilIdentity $ownedFile[0])
                $ownerKey='Software\Kwil\Installer\'+$family+'\Components\'+$Model.Components[$r.component]
                Assert-Kwil ($r.root -ceq 'H' -and $r.relative -ceq $ownerKey -and $r.data -ceq $ownerLiteral) 'Owner literal/key mismatch'
            } else {
                Assert-Kwil ($r.data.Replace('${I}','') -notmatch '[$\{\}]') 'unexpected registry dollar/brace literal'
            }
            $data=$r.data.Replace('${I}','[INSTALLDIR]')
            Assert-Kwil ($data.Length -gt 0 -and -not $data.StartsWith('#')) 'REG_SZ type prefix/empty data refused'
            Add-KwilRow $t Registry @($r.registryKey,1,$r.relative,$name,$data,$r.component)
            # File-marker components each have one R; shell components each have one R.
            Assert-Kwil (-not $keypaths.ContainsKey($r.component)) 'multiple component keypaths'
            $keypaths[$r.component]=$r.registryKey
        }
    }
    foreach($c in $Model.Components.Keys) {
        $dir=if($directories.ContainsKey($c)){$directories[$c]}else{'INSTALLDIR'}
        Add-KwilRow $t Component @($c,$Model.Components[$c],$dir,260,$null,$keypaths[$c])
        Add-KwilRow $t FeatureComponents @('Main',$c)
    }
    $owner=@($Model.Rows | Where-Object { $_.kind -ceq 'F' -and $_.root -ceq 'I' })[0].component
    Add-KwilRow $t RemoveFile @('RemoveInstallDir',$owner,$null,'INSTALLDIR',2)
    Add-KwilRow $t Media @(1,8,$null,'#kwil.cab',$null,$null)
    Add-KwilRow $t Binary @('KwilPreflight',$GuardPath)
    Add-KwilRow $t Icon @('KwilIcon',$IconPath)
    Add-KwilRow $t CustomAction @('KwilPreflight',1,'KwilPreflight','KwilPreflightV2')
    Add-KwilRow $t CustomAction @('SetKwilArpLocation',51,'ARPINSTALLLOCATION','[INSTALLDIR]')
    $actions=[ordered]@{FindRelatedProducts=25;CostInitialize=800;FileCost=900;CostFinalize=1000;SetKwilArpLocation=1010;MigrateFeatureStates=1200;InstallValidate=1400;KwilPreflight=1490;InstallInitialize=1500;RemoveExistingProducts=1501;ProcessComponents=1600;UnpublishFeatures=1800;RemoveRegistryValues=2600;RemoveFiles=3500;InstallFiles=4000;WriteRegistryValues=5000;RegisterUser=6000;RegisterProduct=6100;PublishFeatures=6300;PublishProduct=6400;InstallFinalize=6600}
    foreach($a in $actions.Keys){$condition=if($a -cin @('KwilPreflight','SetKwilArpLocation')){'1'}else{$null}; Add-KwilRow $t InstallExecuteSequence @($a,$condition,$actions[$a])}
    Add-KwilRow $t InstallExecuteSequence @('LaunchConditions',$null,100)
    Add-KwilRow $t LaunchCondition @('NOT RollbackDisabled AND NOT DISABLEROLLBACK','Kwil requires Windows Installer rollback. Enable rollback and retry.')
    Add-KwilRow $t Upgrade @($Metadata.familyGuid,$null,$Metadata.msiVersion,$null,1,'ALL','KWIL_OLDER')
    Add-KwilRow $t Upgrade @($Metadata.familyGuid,$Metadata.msiVersion,$Metadata.msiVersion,$null,770,$null,'KWIL_EQUAL')
    Add-KwilRow $t Upgrade @($Metadata.familyGuid,$Metadata.msiVersion,$null,$null,2,$null,'KWIL_NEWER')
    Add-KwilRow $t Error @(25001,'Kwil installer preflight: [2]. [3]')
    Add-KwilRow $t KwilPackage @(1,2,1,$Metadata.familyGuid,$Metadata.architecture,$Metadata.releaseVersion,$Metadata.channel,$Metadata.msiVersion,$Metadata.commit,$Metadata.target,2,$Metadata.guard.sha256,$InventoryHash,$BuildId,$ProductCode,$PackageCode,29)
    foreach($im in $Metadata.guard.imports){Add-KwilRow $t KwilGuardImport @($im.load,$im.dll)}
    foreach($table in $t.Keys) {
        foreach($c in $t[$table].Columns) {
            $nullable=if([char]::IsUpper($c.Type[0])){'Y'}else{'N'}
            $category=switch($c.Type[0].ToString().ToLowerInvariant()){'i'{if($c.Type.Substring(1) -ceq '4'){'DoubleInteger'}else{'Integer'}};'v'{'Binary'};default{'Text'}}
            Add-KwilRow $t _Validation @($table,$c.Name,$nullable,$null,$null,$null,$null,$category,$null,$null)
        }
    }
    return $t
}

function Release-KwilCom {
    param($Object)
    if($null -ne $Object -and [Runtime.InteropServices.Marshal]::IsComObject($Object)){ $null=[Runtime.InteropServices.Marshal]::FinalReleaseComObject($Object) }
}

function Get-KwilComField {
    param($Record,[int]$Index,[bool]$Integer=$false)
    if($Record.GetType().InvokeMember('IsNull',[Reflection.BindingFlags]::GetProperty,$null,$Record,@($Index))){return $null}
    $name=if($Integer){'IntegerData'}else{'StringData'}
    return $Record.GetType().InvokeMember($name,[Reflection.BindingFlags]::GetProperty,$null,$Record,@($Index))
}

function Set-KwilComField {
    param($Record,[int]$Index,$Value,[string]$Type)
    if($null -eq $Value){return}
    if($Type[0] -eq 'v'){$null=$Record.SetStream($Index,[string]$Value);return}
    $name=if($Type[0].ToString().ToLowerInvariant() -ceq 'i'){'IntegerData'}else{'StringData'}
    $null=$Record.GetType().InvokeMember($name,[Reflection.BindingFlags]::SetProperty,$null,$Record,@($Index,$Value))
}

function Invoke-KwilSql {
    param($Database,[string]$Sql,$Record=$null)
    $v=$Database.OpenView($Sql)
    # COM procedures may emit null/automation return values into PowerShell's
    # success stream. This helper has no data result, including during cleanup.
    try {$null=$v.Execute($Record)}finally{if($null -ne $v){$null=$v.Close();Release-KwilCom $v}}
}

function New-KwilDatabase {
    param($Installer,[string]$Path,$Tables,[string]$Cabinet,[string]$Architecture,[string]$PackageCode)
    $db=$Installer.OpenDatabase($Path,3)
    try {
        foreach($table in $Tables.Keys) {
            $defs=@(foreach($c in $Tables[$table].Columns) {
                $type=switch($c.Type[0].ToString().ToLowerInvariant()){'i'{if($c.Type.Substring(1) -ceq '2'){'SHORT'}else{'LONG'}};'v'{'OBJECT'};default{'CHAR('+$c.Type.Substring(1)+')'}}
                $nn=if([char]::IsLower($c.Type[0])){' NOT NULL'}else{''}
                '`'+$c.Name+'` '+$type+$nn
            })
            $pk=(@($Tables[$table].Keys | ForEach-Object {'`'+$_+'`'}) -join ', ')
            Invoke-KwilSql $db ('CREATE TABLE `'+$table+'` ('+($defs -join ', ')+' PRIMARY KEY '+$pk+')')
        }
        foreach($table in $Tables.Keys) {
            $spec=$Tables[$table]; $cols=(@($spec.Columns | ForEach-Object {'`'+$_.Name+'`'}) -join ', ')
            $marks=(@($spec.Columns | ForEach-Object {'?'}) -join ', ')
            $view=$db.OpenView('INSERT INTO `'+$table+'` ('+$cols+') VALUES ('+$marks+')')
            try {
                foreach($row in $spec.Rows) {
                    $r=$Installer.CreateRecord($spec.Columns.Count)
                    try {for($i=0;$i -lt $spec.Columns.Count;$i++){Set-KwilComField $r ($i+1) $row[$i] $spec.Columns[$i].Type}; $null=$view.Execute($r)}finally{Release-KwilCom $r}
                }
            }finally{$null=$view.Close();Release-KwilCom $view}
        }
        $r=$Installer.CreateRecord(2)
        try{Set-KwilComField $r 1 'kwil.cab' 's';Set-KwilComField $r 2 $Cabinet 'v';Invoke-KwilSql $db 'INSERT INTO `_Streams` (`Name`,`Data`) VALUES (?,?)' $r}finally{Release-KwilCom $r}
        $summary=$db.SummaryInformation(5)
        try {
            foreach($entry in @(@(1,1252),@(7,$(if($Architecture -ceq 'x64'){'x64;1033'}else{'Arm64;1033'})),@(9,$PackageCode),@(14,500),@(15,10))) {
                $null=$summary.GetType().InvokeMember('Property',[Reflection.BindingFlags]::SetProperty,$null,$summary,@($entry[0],$entry[1]))
            }
            $null=$summary.Persist()
        }finally{Release-KwilCom $summary}
        $null=$db.Commit()
    }finally{Release-KwilCom $db}
}

function Get-KwilRowSignature {
    param([object[]]$Row)
    $s=[IO.MemoryStream]::new();$w=[IO.BinaryWriter]::new($s,[Text.Encoding]::UTF8,$true)
    try {
        foreach($v in $Row){if($null -eq $v){$w.Write([byte]0)}else{$w.Write([byte]1);Write-KwilLP $w ([string]$v)}}
        return [Convert]::ToBase64String($s.ToArray())
    }finally{$w.Dispose();$s.Dispose()}
}

function Read-KwilQuery {
    param($Database,[string]$Sql,[bool[]]$IntegerColumns,[int]$Maximum)
    Assert-Kwil ($IntegerColumns.Count -ge 1 -and $IntegerColumns.Count -le 32) 'MSI readback column limit'
    $v=$Database.OpenView($Sql);$rows=[Collections.Generic.List[object]]::new()
    try {
        $null=$v.Execute()
        while($null -ne ($r=$v.Fetch())) {
            try {
                Assert-Kwil ($rows.Count -lt $Maximum) 'MSI readback row limit'
                # Assign cells directly. A nullable SQL cell must remain one
                # slot; neither nulls nor COM procedure results are row output.
                $row=[object[]]::new($IntegerColumns.Count)
                for($i=0;$i -lt $IntegerColumns.Count;$i++) {
                    $row[$i]=Get-KwilComField $r ($i+1) $IntegerColumns[$i]
                    if($row[$i] -is [string]){Assert-Kwil ($row[$i].Length -le 32767) 'MSI readback string limit'}
                }
                $rows.Add($row)
            }finally{Release-KwilCom $r}
        }
    }finally{$null=$v.Close();Release-KwilCom $v}
    return ,$rows.ToArray()
}

function Get-KwilMsiColumnType {
    param([string]$Definition,[bool]$PrimaryKey=$false)
    Assert-Kwil ($Definition -cmatch '\A([sS](0|[1-9][0-9]{0,2})|[iI][24]|[vV]0)\z') 'unknown MSI column definition'
    $width=[int]$Definition.Substring(1)
    $type=0x100
    switch($Definition[0].ToString().ToLowerInvariant()) {
        's' {Assert-Kwil ($width -le 255) 'MSI column string width limit';$type=$type -bor 0xc00 -bor $width}
        'v' {$type=$type -bor 0x800}
        'i' {
            $type=$type -bor $width
            # MSI SQL SHORT persists as 0x0502, LONG as 0x0104. The 0x0400
            # SHORT storage flag is part of the canonical type, not a bit to
            # ignore in observed metadata. Nullable/key flags remain exact.
            if($width -eq 2){$type=$type -bor 0x400}
        }
    }
    if([char]::IsUpper($Definition[0])){$type=$type -bor 0x1000}
    if($PrimaryKey){$type=$type -bor 0x2000}
    return $type
}

function Assert-KwilColumnSchema {
    param([string]$Table,$Specification,[object[]]$Columns)
    Assert-Kwil ($Columns.Count -eq $Specification.Columns.Count) "MSI column count $Table"
    for($i=0;$i -lt $Columns.Count;$i++) {
        $c=$Specification.Columns[$i]
        $type=Get-KwilMsiColumnType $c.Type ($Specification.Keys -ccontains $c.Name)
        Assert-Kwil ($Columns[$i].Count -eq 3) "MSI column metadata width $Table"
        $observed=[int]$Columns[$i][2]
        Assert-Kwil ($Columns[$i][0] -eq $i+1 -and $Columns[$i][1] -ceq $c.Name -and $observed -eq $type) ("MSI typed schema mismatch $Table/$($c.Name); expectedType=0x{0:X4} actualType=0x{1:X4}" -f $type,$observed)
    }
}

function Assert-KwilDatabase {
    param($Installer,[string]$Path,$Tables,[string]$Architecture,[string]$PackageCode)
    $db=$Installer.OpenDatabase($Path,0)
    try {
        $names=Read-KwilQuery $db 'SELECT `Name` FROM `_Tables`' @($false) 64
        foreach($row in $names){Assert-Kwil ($Tables.Contains($row[0])) "unexpected MSI table $($row[0])"}
        Assert-Kwil ($names.Count -eq $Tables.Count) 'missing MSI tables'
        foreach($table in $Tables.Keys) {
            $spec=$Tables[$table]
            $columns=Read-KwilQuery $db ('SELECT `Number`,`Name`,`Type` FROM `_Columns` WHERE `Table`='''+$table+''' ORDER BY `Number`') @($true,$false,$true) 32
            Assert-KwilColumnSchema $table $spec $columns
            if($table -cin @('Binary','Icon')) {
                $rows=Read-KwilQuery $db ('SELECT `Name` FROM `'+$table+'`') @($false) 2
                Assert-Kwil ($rows.Count -eq 1 -and $rows[0][0] -ceq $spec.Rows[0][0]) "MSI stream key mismatch $table"
                continue
            }
            $colNames=(@($spec.Columns | ForEach-Object{'`'+$_.Name+'`'}) -join ',')
            $ints=@($spec.Columns | ForEach-Object{$_.Type[0].ToString().ToLowerInvariant() -ceq 'i'})
            $rows=Read-KwilQuery $db ('SELECT '+$colNames+' FROM `'+$table+'`') $ints 512
            Assert-Kwil ($rows.Count -eq $spec.Rows.Count) "MSI row count $table"
            $expected=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
            foreach($row in $spec.Rows){Assert-Kwil ($expected.Add((Get-KwilRowSignature $row))) "duplicate expected row $table"}
            foreach($row in $rows){Assert-Kwil ($expected.Remove((Get-KwilRowSignature $row))) "MSI row mismatch $table"}
            Assert-Kwil ($expected.Count -eq 0) "MSI omitted rows $table"
        }
        $summary=$db.SummaryInformation(0)
        try {
            foreach($entry in @(@(1,1252),@(7,$(if($Architecture -ceq 'x64'){'x64;1033'}else{'Arm64;1033'})),@(9,$PackageCode),@(14,500),@(15,10))) {
                $v=$summary.GetType().InvokeMember('Property',[Reflection.BindingFlags]::GetProperty,$null,$summary,@($entry[0]))
                Assert-Kwil ([string]$v -ceq [string]$entry[1]) "MSI summary mismatch $($entry[0])"
            }
        }finally{Release-KwilCom $summary}
        # Reconstruct identity from actual typed rows, independently of table serialization.
        $pr=Read-KwilQuery $db 'SELECT `Family`,`Architecture`,`ReleaseVersion`,`Channel`,`MsiVersion`,`Commit`,`Target`,`GuardSha256`,`InventorySha256`,`BuildId`,`ProductCode` FROM `KwilPackage`' (@($false)*11) 1
        $rr=Read-KwilQuery $db 'SELECT `Id`,`Kind`,`Root`,`Relative`,`Name`,`Data`,`Size`,`Sha256`,`Component_`,`Source`,`File_`,`Registry_`,`ValueType` FROM `KwilResource` ORDER BY `Id`' (@($false)*12+@($true)) 29
        $fields=@('id','kind','root','relative','name','data','size','sha256','component','source','fileKey','registryKey','valueType')
        $resource=@(foreach($row in $rr){$r=[ordered]@{};for($i=0;$i -lt 13;$i++){$r[$fields[$i]]=[string]$row[$i]};$r})
        $ii=Read-KwilQuery $db 'SELECT `Load`,`Dll` FROM `KwilGuardImport` ORDER BY `Load`,`Dll`' @($false,$false) 128
        $im=@(foreach($row in $ii){[ordered]@{load=$row[0];dll=$row[1]}})
        $m=[ordered]@{familyGuid=$pr[0][0];architecture=$pr[0][1];releaseVersion=$pr[0][2];channel=$pr[0][3];msiVersion=$pr[0][4];commit=$pr[0][5];target=$pr[0][6];guard=[ordered]@{sha256=$pr[0][7];imports=$im}}
        $inventory=Get-KwilInventoryBytes $resource; $build=Get-KwilBuildId $m $inventory
        $product=New-KwilUuid5 '{918F53A7-7A40-5E54-96EB-E2FD9B0C605B}' (New-KwilEncoding 'KWP2' @($m.architecture,$m.releaseVersion,$m.channel,$m.msiVersion,$build))
        Assert-Kwil ((Get-KwilHash $inventory) -ceq $pr[0][8] -and $build -ceq $pr[0][9] -and $product -ceq $pr[0][10]) 'MSI readback identity reconstruction mismatch'
    }finally{Release-KwilCom $db}
}

function Export-KwilMsiStream {
    param([string]$Msi,[string]$Sql,[string]$Destination,[long]$Maximum)
    [uint32]$db=0;[uint32]$view=0;[uint32]$record=0;$out=$null
    try {
        Assert-Kwil ([KwilMsi.Native]::MsiOpenDatabaseW($Msi,[IntPtr]::Zero,[ref]$db) -eq 0) 'MSI stream read open failed'
        Assert-Kwil ([KwilMsi.Native]::MsiDatabaseOpenViewW($db,$Sql,[ref]$view) -eq 0) 'MSI stream query failed'
        Assert-Kwil ([KwilMsi.Native]::MsiViewExecute($view,0) -eq 0 -and [KwilMsi.Native]::MsiViewFetch($view,[ref]$record) -eq 0) 'MSI stream missing'
        $out=[IO.File]::Open($Destination,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
        $buffer=[byte[]]::new(65536);[long]$total=0
        do {
            [uint32]$n=65536
            Assert-Kwil ([KwilMsi.Native]::MsiRecordReadStream($record,1,$buffer,[ref]$n) -eq 0) 'MSI stream read error'
            Assert-Kwil ($n -le 65536 -and $total -le $Maximum-$n) 'MSI stream byte limit'
            $out.Write($buffer,0,[int]$n);$total+=$n
        }while($n -gt 0)
    }finally{
        if($null -ne $out){$out.Dispose()}
        foreach($h in @($record,$view,$db)){if($h -ne 0){$null=[KwilMsi.Native]::MsiCloseHandle($h)}}
    }
}

function Read-KwilSmallInput {
    param($InputFile,[int]$Maximum=1048576)
    Assert-Kwil ($InputFile.Size -le $Maximum) 'small input byte limit'
    $b=[byte[]]::new([int]$InputFile.Size); $InputFile.Stream.Position=0; $done=0
    while($done -lt $b.Length){$n=$InputFile.Stream.Read($b,$done,[Math]::Min(65536,$b.Length-$done));Assert-Kwil ($n -gt 0) 'small input EOF';$done+=$n}
    return ,$b
}

function Get-KwilDirectoryPath {
    param([string]$Path)
    $null=Assert-KwilLocalPath $Path $false $true
    $h=[KwilMsi.Native]::CreateFileW($Path,0x80,1,[IntPtr]::Zero,3,0x02200000,[IntPtr]::Zero)
    try{Assert-Kwil (-not $h.IsInvalid) 'directory identity open failed';return Get-KwilFinalPath $h}finally{$h.Dispose()}
}

function Assert-KwilPayloadListing {
    param([string]$Path)
    $allowed=@('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update','.installer')
    $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($p in [IO.Directory]::EnumerateFileSystemEntries($Path)) {
        $name=[IO.Path]::GetFileName($p)
        Assert-Kwil ($seen.Count -lt 8 -and $allowed -ccontains $name -and $seen.Add($name)) "undeclared MSI staging entry $name"
        $null=Assert-KwilLocalPath $p $false ($name -ceq '.installer')
    }
    Assert-Kwil ($seen.Count -eq 8) 'incomplete MSI payload listing'
    $seen.Clear()
    foreach($p in [IO.Directory]::EnumerateFileSystemEntries([IO.Path]::Combine($Path,'.installer'))) {
        $name=[IO.Path]::GetFileName($p)
        Assert-Kwil ($seen.Count -lt 2 -and @('input.json','kwil-installer-guard.dll') -ccontains $name -and $seen.Add($name)) 'undeclared installer metadata entry'
        $null=Assert-KwilLocalPath $p
    }
    Assert-Kwil ($seen.Count -eq 2) 'missing installer metadata or guard'
}

function Write-KwilNewBytes {
    param([string]$Path,[byte[]]$Bytes)
    $s=[IO.File]::Open($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
    try{$s.Write($Bytes,0,$Bytes.Length);$s.Flush($true)}finally{$s.Dispose()}
}

function Copy-KwilInput {
    param($InputFile,[string]$Path)
    $s=[IO.File]::Open($Path,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
    try{$InputFile.Stream.Position=0;$InputFile.Stream.CopyTo($s,65536);$s.Flush($true)}finally{$s.Dispose()}
}

function Write-KwilReceipt {
    param($Context,[string]$Message)
    $line=([DateTime]::UtcNow.ToString('o',[Globalization.CultureInfo]::InvariantCulture))+' '+$Message
    [Console]::Out.WriteLine($line)
    [IO.File]::AppendAllText([IO.Path]::Combine($Context.Work,'processes.log'),$line+"`n",[Text.UTF8Encoding]::new($false))
}

function Assert-KwilDeadline {
    param($Context)
    Assert-Kwil ($Context.Watch.ElapsedMilliseconds -lt 120000) 'backend 120-second deadline exceeded; retain work and inspect PID receipts'
}

function Wait-KwilPackagingChild {
    param($Context,$Process,[string]$Tag,[string]$Tool,$Timer,
        [scriptblock]$ReceiptWriter={param($ReceiptContext,$Message);Write-KwilReceipt $ReceiptContext $Message})
    # Production passes ONLY the Process it just started and its own stopwatch.
    # The writer/clock arguments let memory fixtures inject IO failure and time
    # expiry without starting or terminating a real process.
    $childId=$Process.Id
    $state=[pscustomobject]@{ReceiptFailed=$false}
    $emit={
        param([string]$Message)
        try {$null=& $ReceiptWriter $Context $Message}
        catch {
            $state.ReceiptFailed=$true
            # A failed receipt must never escape past the bounded wait/cleanup.
            # Console reporting is best effort too; the packaging result fails.
            try {[Console]::Error.WriteLine("Kwil MSI: receipt write failed for owned child pid=$childId; bounded wait/timeout cleanup remains active.")}catch{}
        }
    }
    $null=& $emit "child=$Tag pid=$childId timeoutSeconds=60 tool=$Tool"
    $expired=$false
    while(-not $Process.WaitForExit(100)) {
        if($Timer.ElapsedMilliseconds -ge 60000 -or $Context.Watch.ElapsedMilliseconds -ge 119000){$expired=$true;break}
    }
    if($expired) {
        $null=& $emit "child=$Tag pid=$childId timeout=true cleanup=attempt"
        # ROOT clarification: exact owned Process, timeout only, no tree kill.
        try {
            $Process.Kill()
            $closed=$Process.WaitForExit(1000)
            $null=& $emit "child=$Tag pid=$childId cleanupExited=$closed"
        } catch {
            $null=& $emit "child=$Tag pid=$childId cleanupFailed=$($_.Exception.GetType().Name)"
        }
        throw "Kwil MSI: $Tag pid=$childId timed out; failed work retained"
    }
    $null=& $emit "child=$Tag pid=$childId exitCode=$($Process.ExitCode) timeout=false cleanup=not-needed"
    Assert-Kwil (-not $state.ReceiptFailed) "child $Tag pid=$childId exited but process receipt write failed; failed work retained"
}

function Invoke-KwilPackagingChild {
    param($Context,[string]$Tool,[string[]]$Arguments,[string]$Tag)
    Assert-KwilDeadline $Context
    $system=[Environment]::SystemDirectory
    $path=[IO.Path]::Combine($system,$Tool)
    $null=Assert-KwilLocalPath $path
    $si=[Diagnostics.ProcessStartInfo]::new();$si.FileName=$path;$si.WorkingDirectory=$Context.Work
    $si.UseShellExecute=$false;$si.CreateNoWindow=$true;$si.WindowStyle=[Diagnostics.ProcessWindowStyle]::Hidden
    $si.RedirectStandardOutput=$true;$si.RedirectStandardError=$true
    foreach($a in $Arguments){$si.ArgumentList.Add($a)}
    $out=[IO.File]::Open([IO.Path]::Combine($Context.Work,$Tag+'.stdout'),[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
    $err=$null;$p=[Diagnostics.Process]::new();$p.StartInfo=$si;$started=$false
    try {
        $err=[IO.File]::Open([IO.Path]::Combine($Context.Work,$Tag+'.stderr'),[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::Read)
        $started=$p.Start();Assert-Kwil $started "could not start $Tool"
        $timer=[Diagnostics.Stopwatch]::StartNew()
        try {
            $ot=[KwilMsi.Native]::Drain($p.StandardOutput.BaseStream,$out)
            $et=[KwilMsi.Native]::Drain($p.StandardError.BaseStream,$err)
        } finally {
            # Even drain setup or receipt IO failure cannot skip waiting for
            # this exact started child, or its timeout-only cleanup.
            $null=Wait-KwilPackagingChild $Context $p $Tag $Tool $timer
        }
        Assert-Kwil ($ot.Wait(1000) -and $et.Wait(1000)) 'packaging log drain timeout'
        Assert-Kwil ($ot.Result -and $et.Result) 'packaging output exceeded 1 MiB per stream'
        Assert-Kwil ($p.ExitCode -eq 0) "$Tool failed; inspect retained $Tag logs"
    }finally{
        # Never kill on an unrelated exception; the retained receipt identifies
        # any still-running exact child for ROOT. All cleanup kills above are timeout-only.
        $p.Dispose();$out.Dispose();if($null -ne $err){$err.Dispose()}
    }
}

function Assert-KwilCabinetHeader {
    param([IO.Stream]$Stream,[object[]]$Files)
    Assert-Kwil ($Stream.Length -le 1090519040) 'CAB byte limit'
    $h=Read-KwilAt $Stream 0 36
    Assert-Kwil ([Text.Encoding]::ASCII.GetString($h,0,4) -ceq 'MSCF') 'CAB signature mismatch'
    Assert-Kwil ([BitConverter]::ToUInt32($h,8) -eq $Stream.Length -and [BitConverter]::ToUInt16($h,26) -eq 1 -and [BitConverter]::ToUInt16($h,28) -eq $Files.Count -and [BitConverter]::ToUInt16($h,30) -eq 0 -and [BitConverter]::ToUInt16($h,34) -eq 0) 'CAB count/flags/size mismatch'
    $position=[long][BitConverter]::ToUInt32($h,16)
    Assert-Kwil ($position -ge 44) 'CAB file table offset invalid'
    [long]$uncompressed=0
    foreach($f in $Files) {
        $r=Read-KwilAt $Stream $position 16;$position+=16
        Assert-Kwil ([BitConverter]::ToUInt32($r,0) -eq [long]$f.size -and [BitConverter]::ToUInt32($r,4) -eq $uncompressed -and [BitConverter]::ToUInt16($r,8) -eq 0) 'CAB member size/folder/offset mismatch'
        $name=Read-KwilAt $Stream $position 8;$position+=8
        Assert-Kwil ($name[7] -eq 0 -and [Text.Encoding]::ASCII.GetString($name,0,7) -ceq $f.fileKey -and $f.fileKey -cmatch '^F[0-9]{6}$') 'CAB member name/order mismatch'
        $uncompressed+=[long]$f.size
        Assert-Kwil ($uncompressed -le 1073741824) 'CAB expanded byte limit'
    }
}

function Assert-KwilExtractedFiles {
    param([string]$Directory,[object[]]$Files,[string]$Architecture)
    $seen=[Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach($path in [IO.Directory]::EnumerateFileSystemEntries($Directory)) {
        Assert-Kwil ($seen.Count -lt 8) 'CAB extraction entry limit'
        $name=[IO.Path]::GetFileName($path)
        $expected=@($Files | Where-Object{$_.fileKey -ceq $name})
        Assert-Kwil ($expected.Count -eq 1 -and $seen.Add($name)) 'foreign CAB extraction entry'
        $f=Open-KwilInput $path 536870912
        try {
            Assert-Kwil ($f.Size -eq [long]$expected[0].size -and $f.Sha256 -ceq $expected[0].sha256) 'CAB extracted bytes mismatch'
            if($expected[0].relative -cin @('kwil.exe','kwil.com')) {
                $kind=if($expected[0].relative -ceq 'kwil.exe'){'exe'}else{'com'}
                $null=Get-KwilPeImports $f.Stream $Architecture $kind
            }
        }finally{$f.Stream.Dispose()}
    }
    Assert-Kwil ($seen.Count -eq $Files.Count) 'CAB omitted files'
}

function Remove-KwilSuccessfulWork {
    param($Context,[string[]]$OwnedFiles,[string[]]$ExtractedFiles)
    # No recursive delete. Validate every exact target first; never remove an
    # unexpected file even inside the randomly created owned directory.
    $work=Assert-KwilLocalPath $Context.Work $false $true
    $extract=[IO.Path]::Combine($work,'extract')
    $null=Assert-KwilLocalPath $extract $false $true
    $allowed=[Collections.Generic.HashSet[string]]::new($OwnedFiles,[StringComparer]::Ordinal)
    foreach($p in [IO.Directory]::EnumerateFileSystemEntries($work)) {
        $name=[IO.Path]::GetFileName($p)
        if($name -ceq 'extract'){continue}
        Assert-Kwil ($allowed.Contains($name)) 'unexpected work entry; retain work'
        $null=Assert-KwilLocalPath $p
    }
    foreach($p in [IO.Directory]::EnumerateFileSystemEntries($extract)) {
        Assert-Kwil ($ExtractedFiles -ccontains [IO.Path]::GetFileName($p)) 'unexpected extracted work entry; retain work'
        $null=Assert-KwilLocalPath $p
    }
    foreach($p in $ExtractedFiles){[IO.File]::Delete([IO.Path]::Combine($extract,$p))}
    [IO.Directory]::Delete($extract,$false)
    foreach($p in $OwnedFiles){[IO.File]::Delete([IO.Path]::Combine($work,$p))}
    [IO.Directory]::Delete($work,$false)
}

function Invoke-KwilMsiBackend {
    param([object[]]$Tokens)
    $ErrorActionPreference='Stop';Set-StrictMode -Version Latest
    $inputs=[Collections.Generic.List[object]]::new();$context=$null;$installer=$null
    $watch=[Diagnostics.Stopwatch]::StartNew()
    try {
        $a=Get-KwilArguments $Tokens
        Assert-Kwil $IsWindows 'Windows with PowerShell 7.2 or newer is required'
        Initialize-KwilNative
        $payload=Assert-KwilLocalPath $a.PayloadDirectory $false $true
        $output=Assert-KwilLocalPath $a.OutputPath $true
        Assert-Kwil ([IO.Path]::GetExtension($output) -ieq '.msi') 'OutputPath must end in .msi'
        $sidecar=Assert-KwilLocalPath ($output+'.manifest.json') $true
        $icon=Assert-KwilLocalPath $a.IconPath
        Assert-Kwil ([StringComparer]::OrdinalIgnoreCase.Equals($icon,[IO.Path]::Combine($payload,'kwil.ico'))) 'IconPath must be payload kwil.ico'
        $parent=[IO.Path]::GetDirectoryName($output)
        Assert-Kwil ($parent -notmatch '%') 'percent is forbidden in the private cabinet work path'
        $parentFinal=Get-KwilDirectoryPath $parent;$payloadFinal=Get-KwilDirectoryPath $payload
        Assert-Kwil (-not [StringComparer]::OrdinalIgnoreCase.Equals($parentFinal,$payloadFinal) -and -not $parentFinal.StartsWith($payloadFinal+'\',[StringComparison]::OrdinalIgnoreCase)) 'output overlaps immutable payload'
        Assert-KwilPayloadListing $payload
        # Pin the reviewed protocol. Updating this pin requires ROOT's coordinated
        # contract/backend/wrapper/guard review, never automatic compatibility.
        $contract=Open-KwilInput ([IO.Path]::Combine($PSScriptRoot,'windows-installer-contract.json')) 1048576
        $inputs.Add($contract)
        Assert-Kwil ($contract.Sha256 -ceq 'a08a1392c0b81dadb950df7ac16ebeaf98676bdaa68fc20e7c3ca884b04b5cc5') 'frozen corrected JSON contract changed; ask ROOT'
        $decision=Open-KwilInput ([IO.Path]::GetFullPath([IO.Path]::Combine($PSScriptRoot,'..','docs','decisions','0004-windows-installer.md'))) 1048576
        $inputs.Add($decision)
        Assert-Kwil ($decision.Sha256 -ceq '3b196d7fb9810ea498b332a46507ac0ecad5cfc405143520a08c8de9ff2966b8') 'frozen corrected decision changed; ask ROOT'
        $files=[ordered]@{};[long]$total=868
        foreach($name in @('kwil.exe','kwil.com','kwil.ico','LICENSE','LICENSES.md','LICENSES.txt','kwil.no-update')) {
            $f=Open-KwilInput ([IO.Path]::Combine($payload,$name)) 536870912
            $inputs.Add($f);$files.Add($name,$f);$total+=$f.Size
            Assert-Kwil ($total -le 1073741824) 'total installed bytes exceeded'
        }
        $marker=[Text.Encoding]::UTF8.GetBytes("Updates are managed by this distribution. Rebuild/reinstall explicitly.`n")
        Assert-Kwil ($files['kwil.no-update'].Size -eq $marker.Length -and $files['kwil.no-update'].Sha256 -ceq (Get-KwilHash $marker)) 'kwil.no-update bytes mismatch'
        $guard=Open-KwilInput ([IO.Path]::Combine($payload,'.installer','kwil-installer-guard.dll')) 16777216;$inputs.Add($guard)
        $json=Open-KwilInput ([IO.Path]::Combine($payload,'.installer','input.json')) 1048576;$inputs.Add($json)
        $imports=Get-KwilPeImports $guard.Stream $a.Architecture 'guard'
        $null=Get-KwilPeImports $files['kwil.exe'].Stream $a.Architecture 'exe'
        $null=Get-KwilPeImports $files['kwil.com'].Stream $a.Architecture 'com'
        $model=New-KwilModel $a.Architecture $files
        $metadata=Read-KwilJson (Read-KwilSmallInput $json)
        Assert-KwilMetadata $metadata $a $model $guard.Sha256 $imports
        $inventory=Get-KwilInventoryBytes $model.Rows;$inventoryHash=Get-KwilHash $inventory
        $build=Get-KwilBuildId $metadata $inventory
        $product=New-KwilUuid5 '{918F53A7-7A40-5E54-96EB-E2FD9B0C605B}' (New-KwilEncoding 'KWP2' @($a.Architecture,$metadata.releaseVersion,$metadata.channel,$metadata.msiVersion,$build))
        $package=[Guid]::NewGuid().ToString('B').ToUpperInvariant()
        $work=[IO.Path]::Combine($parent,'.kwil-msi-'+$PID+'-'+[Guid]::NewGuid().ToString('N'))
        $null=Assert-KwilLocalPath $work $true
        $null=[IO.Directory]::CreateDirectory($work)
        $context=[pscustomobject]@{Work=$work;Watch=$watch}
        Write-KwilReceipt $context "backendPid=$PID timeoutSeconds=120 contractSha256=$($contract.Sha256)"
        $frows=@($model.Rows | Where-Object{$_.kind -ceq 'F'})
        foreach($f in $frows) {
            $dest=[IO.Path]::Combine($work,$f.fileKey)
            if($f.source -ceq 'p'){Copy-KwilInput $files[$f.relative] $dest}else{Write-KwilNewBytes $dest (New-KwilShortcut)}
        }
        $ddf=@('.OPTION EXPLICIT','.Set Cabinet=ON','.Set Compress=ON','.Set CompressionType=MSZIP','.Set MaxDiskSize=0','.Set MaxCabinetSize=0','.Set FolderSizeThreshold=1073741824','.Set CabinetNameTemplate=kwil.cab','.Set DiskDirectoryTemplate=.','.Set RptFileName=cab.rpt','.Set InfFileName=cab.inf')
        foreach($f in $frows){$ddf+='"'+$f.fileKey+'" "'+$f.fileKey+'"'}
        Write-KwilNewBytes ([IO.Path]::Combine($work,'cab.ddf')) ([Text.Encoding]::ASCII.GetBytes(($ddf -join "`r`n")+"`r`n"))
        Invoke-KwilPackagingChild $context 'makecab.exe' @('/F','cab.ddf') 'makecab'
        $cab=[IO.Path]::Combine($work,'kwil.cab')
        $cf=Open-KwilInput $cab 1090519040
        try{Assert-KwilCabinetHeader $cf.Stream $frows}finally{$cf.Stream.Dispose()}
        Assert-KwilDeadline $context
        $msi=[IO.Path]::Combine($work,'package.msi')
        $tables=New-KwilTables $metadata $model $build $inventoryHash $product $package $guard.Path $icon
        $installer=New-Object -ComObject WindowsInstaller.Installer
        $null=New-KwilDatabase $installer $msi $tables $cab $a.Architecture $package
        $null=Assert-KwilDatabase $installer $msi $tables $a.Architecture $package
        $readCab=[IO.Path]::Combine($work,'readback.cab')
        Export-KwilMsiStream $msi 'SELECT `Data` FROM `_Streams` WHERE `Name`=''kwil.cab''' $readCab 1090519040
        $readGuard=[IO.Path]::Combine($work,'readback.guard')
        Export-KwilMsiStream $msi 'SELECT `Data` FROM `Binary` WHERE `Name`=''KwilPreflight''' $readGuard 16777216
        $readIcon=[IO.Path]::Combine($work,'readback.icon')
        Export-KwilMsiStream $msi 'SELECT `Data` FROM `Icon` WHERE `Name`=''KwilIcon''' $readIcon 536870912
        foreach($pair in @(@($readGuard,$guard),@($readIcon,$files['kwil.ico']))) {
            $f=Open-KwilInput $pair[0] 536870912
            try{Assert-Kwil ($f.Size -eq $pair[1].Size -and $f.Sha256 -ceq $pair[1].Sha256) 'embedded Binary/Icon stream mismatch';if($pair[0] -ceq $readGuard){$readImports=Get-KwilPeImports $f.Stream $a.Architecture 'guard';Assert-Kwil ((Get-KwilRowSignature @($readImports | ForEach-Object{$_.load+':'+$_.dll})) -ceq (Get-KwilRowSignature @($imports | ForEach-Object{$_.load+':'+$_.dll}))) 'embedded guard import mismatch'}}finally{$f.Stream.Dispose()}
        }
        $cf=Open-KwilInput $readCab 1090519040
        try{Assert-KwilCabinetHeader $cf.Stream $frows}finally{$cf.Stream.Dispose()}
        $extract=[IO.Path]::Combine($work,'extract');$null=[IO.Directory]::CreateDirectory($extract)
        Invoke-KwilPackagingChild $context 'expand.exe' @('-F:*',$readCab,$extract) 'expand'
        Assert-KwilExtractedFiles $extract $frows $a.Architecture
        foreach($f in $inputs){Assert-KwilInputStable $f}
        Assert-KwilPayloadListing $payload
        Assert-KwilDeadline $context
        # Preserve exactly the inspected MSI. Move refuses an existing output;
        # no overwrite, re-authoring, signature operation or postinspection edit.
        $before=Open-KwilInput $msi 1124073472
        try{$msiSize=$before.Size;$msiHash=$before.Sha256}finally{$before.Stream.Dispose()}
        $null=Assert-KwilLocalPath $output $true
        $null=Assert-KwilLocalPath $sidecar $true
        [IO.File]::Move($msi,$output,$false)
        $published=Open-KwilInput $output 1124073472
        try{Assert-Kwil ($published.Size -eq $msiSize -and $published.Sha256 -ceq $msiHash) 'published MSI differs from inspected MSI'}finally{$published.Stream.Dispose()}
        $manifest=[ordered]@{schema=2;familyGuid=$metadata.familyGuid;componentSchema=1;architecture=$a.Architecture;target=$metadata.target;releaseVersion=$metadata.releaseVersion;channel=$metadata.channel;msiVersion=$metadata.msiVersion;commit=$a.Commit;buildId=$build;inventorySha256=$inventoryHash;productCode=$product;packageCode=$package;guard=$metadata.guard;inventory=$model.Rows;msi=[ordered]@{fileName=[IO.Path]::GetFileName($output);size=[string]$msiSize;sha256=$msiHash};inspection=[ordered]@{database='passed';cabinet='passed';staticImports='passed'};signed=$false;nativeLifecycle='not-run'}
        $manifestBytes=[Text.UTF8Encoding]::new($false,$true).GetBytes(($manifest | ConvertTo-Json -Depth 8 -Compress)+"`n")
        Assert-Kwil ($manifestBytes.Length -le 1048576) 'sidecar byte limit'
        Write-KwilNewBytes $sidecar $manifestBytes
        $side=Open-KwilInput $sidecar 1048576
        try {
            Assert-Kwil ($side.Sha256 -ceq (Get-KwilHash $manifestBytes)) 'sidecar readback mismatch'
            $doc=[Text.Json.JsonDocument]::Parse([Text.Encoding]::UTF8.GetString((Read-KwilSmallInput $side)))
            try{Assert-Kwil ($doc.RootElement.GetProperty('msi').GetProperty('sha256').GetString() -ceq $msiHash -and $doc.RootElement.GetProperty('nativeLifecycle').GetString() -ceq 'not-run') 'sidecar parsed readback mismatch'}finally{$doc.Dispose()}
        }finally{$side.Stream.Dispose()}
        Assert-KwilDeadline $context
        $owned=@($frows | ForEach-Object{$_.fileKey})+@('processes.log','cab.ddf','cab.rpt','cab.inf','kwil.cab','readback.cab','readback.guard','readback.icon','makecab.stdout','makecab.stderr','expand.stdout','expand.stderr')
        Remove-KwilSuccessfulWork $context $owned @($frows | ForEach-Object{$_.fileKey})
        [Console]::Out.WriteLine("Kwil MSI inspected: $output buildId=$build packageCode=$package nativeLifecycle=not-run")
        return 0
    } catch {
        [Console]::Error.WriteLine($_.Exception.Message)
        [Console]::Error.WriteLine($_.ScriptStackTrace)
        if($null -ne $context){[Console]::Error.WriteLine("Failed owned work retained: $($context.Work); see processes.log. Do not accept partial outputs.")}
        return 1
    } finally {
        Release-KwilCom $installer
        foreach($f in $inputs){$f.Stream.Dispose()}
    }
}

if($MyInvocation.InvocationName -eq '.') {
    if($args.Count -ne 0){throw 'Kwil MSI helper loading accepts no arguments'}
    return
}
exit (Invoke-KwilMsiBackend $args)
