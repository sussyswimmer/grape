<#
.SYNOPSIS
Builds Grape's own bsdtar for Windows x64 (DECISIONS D27).

.DESCRIPTION
Builds bsdtar from libarchive's release tarball plus the patches in
scripts/bsdtar/patches, as pinned in scripts/bsdtar/sources.json, with MSVC
and the static CRT (/MT) and every optional library off. macOS has
scripts/bsdtar/build.sh.

The root folder (default target/bsdtar) holds dl/ (the checked tarball,
kept), work/ (the extract and the build tree, deleted at the end of every
run) and out/ (the results). A full build:
  1. reads and checks sources.json, and finds CMake (on PATH, or Visual
     Studio's own), MSVC, dumpbin and mt.exe through vswhere;
  2. keeps dl/libarchive-<version>.tar.gz if its size and SHA-256 match, else
     downloads it into a new temp folder (TLS 1.2 or later) and checks both
     before anything is extracted;
  3. extracts it into work/src with Windows' own tar.exe;
  4. checks each patch's SHA-256, then applies it with git apply, in order;
  5. configures with CMake (the newest Visual Studio, x64) and builds the
     bsdtar target;
  6. puts bsdtar-x86_64-pc-windows-msvc.exe in out/, with BUILDINFO.txt and
     SHA256SUMS;
  7. checks it (the whole of -CheckOnly): it imports only the DLLs
     sources.json allows (dumpbin /dependents), it is x64, its manifest has
     activeCodePage UTF-8 and longPathAware, its --version line is exactly
     sources.json's with only the allowed tokens, and SHA256SUMS matches. A
     full build also regenerates the copyright lines of the compiled sources
     and compares them with licenses/libarchive/copyright-lines.txt.
  8. With -Install, copies it into apps\desktop\src-tauri\binaries\ for
     `tauri dev`. It never touches sidecars.stamp.

It needs git, and the Visual Studio Build Tools with the C++ workload (the
Dev setup in docs/PLATFORM_WINDOWS.md installs them, CMake included). It
never changes PATH and installs nothing. Works in Windows PowerShell 5.1 and
PowerShell 7.

.PARAMETER Root
The folder for dl/, work/ and out/. Default: target/bsdtar in the repo.

.PARAMETER CheckOnly
Check the binary already in <Root>/out and build nothing.

.PARAMETER Install
Also copy the result into apps\desktop\src-tauri\binaries\.

.EXAMPLE
.\scripts\bsdtar\build.ps1 -Install

.EXAMPLE
.\scripts\bsdtar\build.ps1 -Root target/bsdtar -CheckOnly
#>
[CmdletBinding()]
param(
    [string]$Root,
    [switch]$CheckOnly,
    [switch]$Install
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
# Windows PowerShell's progress bar makes downloads many times slower.
$ProgressPreference = 'SilentlyContinue'

$me = 'bsdtar build'
$triple = 'x86_64-pc-windows-msvc'
$exeName = "bsdtar-$triple.exe"
# The only tokens allowed after "libarchive <version>" in the version line.
$allowedTokens = @('libb2/bundled')
$heading = "Copyright lines of the libarchive and bsdtar source files compiled into Grape's " +
    "bsdtar (scripts/bsdtar/build.sh and build.ps1 regenerate this list from the pinned " +
    "source and fail if it differs). Most are holders of the BSD-2-Clause notice in " +
    "LICENSE.txt; the parts under other terms have their full notices in the other files here."

function Fail([string]$Message) {
    throw $Message
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Write-Utf8([string]$Path, [string]$Text) {
    # LF and no BOM, the same as build.sh writes.
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding $false))
}

function Read-Utf8([string]$Path) {
    # Strict: a file that isn't UTF-8 is an error, as in build.sh.
    $strict = New-Object System.Text.UTF8Encoding $false, $true
    return $strict.GetString([System.IO.File]::ReadAllBytes($Path))
}

# The value of one key of a JSON object, or $null. Keys are case-sensitive
# here, as in JSON, although PowerShell's objects aren't.
function Get-Key($Obj, [string]$Name) {
    foreach ($p in $Obj.PSObject.Properties) {
        if ($p.Name -ceq $Name) { return $p.Value }
    }
    return $null
}

function Test-Keys($Obj, [string]$Where, [string[]]$Required) {
    if ($Obj -isnot [System.Management.Automation.PSCustomObject]) {
        Fail "scripts/bsdtar/sources.json: $Where must be an object."
    }
    foreach ($p in $Obj.PSObject.Properties) {
        if (-not $p.Name.StartsWith('_') -and $Required -cnotcontains $p.Name) {
            Fail "scripts/bsdtar/sources.json: $Where has an unknown key '$($p.Name)'."
        }
    }
    foreach ($key in $Required) {
        if ($null -eq (Get-Key $Obj $key)) {
            Fail "scripts/bsdtar/sources.json: $Where is missing '$key'."
        }
    }
}

function Test-String($Value, [string]$Where, [string]$Pattern, [string]$Hint) {
    if ($Value -isnot [string] -or $Value -cnotmatch $Pattern) {
        Fail "scripts/bsdtar/sources.json: $Where $Hint."
    }
    return $Value
}

# Reads and checks sources.json: the same rules as build.sh.
function Read-Sources([string]$Path) {
    try {
        $s = [System.IO.File]::ReadAllText($Path) | ConvertFrom-Json
    } catch {
        Fail "scripts/bsdtar/sources.json: can't read it as JSON ($($_.Exception.Message))."
    }
    Test-Keys $s 'the file' @('format', 'libarchive', 'marker', 'patches', 'version_line',
        'macos_deployment_target', 'windows_dlls')
    $format = Get-Key $s 'format'
    if (-not ($format -is [int] -or $format -is [long]) -or $format -ne 1) {
        Fail "scripts/bsdtar/sources.json: 'format' must be 1. This script reads format 1 only."
    }

    $la = Get-Key $s 'libarchive'
    Test-Keys $la 'libarchive' @('version', 'released', 'url', 'sha256', 'size')
    $version = Test-String (Get-Key $la 'version') 'libarchive.version' '^[0-9]+(\.[0-9]+)*$' 'must look like 3.8.10'
    $null = Test-String (Get-Key $la 'released') 'libarchive.released' '^[0-9]{4}-[0-9]{2}-[0-9]{2}$' 'must be a date like 2026-10-14'
    $url = Test-String (Get-Key $la 'url') 'libarchive.url' '^https://\S+$' 'must be an https:// URL'
    if (-not $url.EndsWith("/libarchive-$version.tar.gz", [System.StringComparison]::Ordinal)) {
        Fail "scripts/bsdtar/sources.json: libarchive.url must end in /libarchive-$version.tar.gz."
    }
    $sha = Test-String (Get-Key $la 'sha256') 'libarchive.sha256' '^[0-9a-f]{64}$' 'must be 64 lower-case hex digits'
    $size = Get-Key $la 'size'
    if (-not ($size -is [int] -or $size -is [long]) -or $size -le 0) {
        Fail "scripts/bsdtar/sources.json: libarchive.size must be the tarball's size in bytes."
    }

    $marker = Test-String (Get-Key $s 'marker') 'marker' '^grape\.[0-9]+$' 'must look like grape.1'

    $list = Get-Key $s 'patches'
    if ($list -isnot [System.Array] -or $list.Count -eq 0) {
        Fail 'scripts/bsdtar/sources.json: patches must be a non-empty list.'
    }
    $patches = @()
    $names = @()
    for ($i = 0; $i -lt $list.Count; $i++) {
        $p = $list[$i]
        $where = "patches[$i]"
        Test-Keys $p $where @('file', 'sha256', 'from')
        $name = Test-String (Get-Key $p 'file') "$where.file" '^[A-Za-z0-9][A-Za-z0-9._-]*\.patch$' 'must be a plain .patch file name (no /, \ or ..)'
        if ($name.Contains('..')) {
            Fail "scripts/bsdtar/sources.json: $where.file must not contain '..'."
        }
        if ($names -ccontains $name) {
            Fail "scripts/bsdtar/sources.json: $where.file names '$name' twice."
        }
        $names += $name
        $psha = Test-String (Get-Key $p 'sha256') "$where.sha256" '^[0-9a-f]{64}$' 'must be 64 lower-case hex digits'
        $null = Test-String (Get-Key $p 'from') "$where.from" '^\S' 'must say where the patch comes from'
        $patches += [pscustomobject]@{ File = $name; Sha256 = $psha }
    }

    $lines = Get-Key $s 'version_line'
    Test-Keys $lines 'version_line' @('windows', 'macos', 'linux')
    $prefix = "bsdtar $version-$marker - libarchive $version "
    foreach ($key in 'windows', 'macos', 'linux') {
        $line = Get-Key $lines $key
        if ($line -isnot [string] -or -not $line.StartsWith($prefix, [System.StringComparison]::Ordinal) -or
            $line.Contains("`t") -or $line.Contains("`n")) {
            Fail "scripts/bsdtar/sources.json: version_line.$key must start with '$prefix' (trailing space included)."
        }
    }
    $null = Test-String (Get-Key $s 'macos_deployment_target') 'macos_deployment_target' '^[0-9]+\.[0-9]+$' 'must look like 13.0'
    $dlls = Get-Key $s 'windows_dlls'
    if ($dlls -isnot [System.Array] -or $dlls.Count -eq 0) {
        Fail 'scripts/bsdtar/sources.json: windows_dlls must be a non-empty list.'
    }
    for ($i = 0; $i -lt $dlls.Count; $i++) {
        $null = Test-String $dlls[$i] "windows_dlls[$i]" '^[A-Za-z0-9_.-]+\.[Dd][Ll][Ll]$' "must be a DLL's file name"
    }

    return [pscustomobject]@{
        Version     = $version
        Url         = $url
        Sha256      = $sha
        Size        = [long]$size
        Marker      = $marker
        Patches     = $patches
        VersionLine = [string](Get-Key $lines 'windows')
        Dlls        = @($dlls | ForEach-Object { [string]$_ })
    }
}

# Runs a program with native stderr kept as text, never as PowerShell
# errors. Returns its output lines and exit code.
function Invoke-Captured([string]$Exe, [string[]]$Arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $lines = @(& $Exe @Arguments 2>&1 | ForEach-Object { [string]$_ })
        $code = $LASTEXITCODE
    } catch {
        Fail "$([System.IO.Path]::GetFileName($Exe)) couldn't start ($($_.Exception.GetBaseException().Message))."
    } finally {
        $ErrorActionPreference = $previous
    }
    return [pscustomobject]@{ Lines = $lines; Code = $code }
}

# Runs a program with its output going to the console (CMake, the build).
function Invoke-Streamed([string]$Exe, [string[]]$Arguments, [string]$What) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Exe @Arguments 2>&1 | ForEach-Object { Write-Host ([string]$_) }
        $code = $LASTEXITCODE
    } catch {
        Fail "$What couldn't start ($($_.Exception.GetBaseException().Message))."
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($code -ne 0) { Fail "$What failed (exit code $code)." }
}

# vswhere has a fixed path; GitHub's Windows images don't put it on PATH.
function Get-VsWhere {
    $base = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrEmpty($base)) { Fail "ProgramFiles(x86) isn't set, so vswhere can't be found." }
    $vswhere = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) {
        Fail "$vswhere is missing. Install the Visual Studio Build Tools with the C++ workload (docs/PLATFORM_WINDOWS.md, Dev setup)."
    }
    return $vswhere
}

# The newest file vswhere finds for a Visual Studio with the MSVC x64 tools.
function Find-VsFile([string]$VsWhere, [string]$Pattern, [string]$Requires) {
    $r = Invoke-Captured $VsWhere @('-latest', '-products', '*', '-requires', $Requires, '-find', $Pattern)
    $found = @($r.Lines | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) })
    if ($r.Code -ne 0 -or $found.Count -eq 0) { return $null }
    return $found[$found.Count - 1]
}

function Get-Tools([bool]$ForBuild) {
    $vswhere = Get-VsWhere
    $msvc = 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64'
    $dumpbin = Find-VsFile $vswhere 'VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe' $msvc
    if ($null -eq $dumpbin) {
        Fail "MSVC's x64 tools (dumpbin.exe) aren't installed. Install the Visual Studio Build Tools with the C++ workload (docs/PLATFORM_WINDOWS.md, Dev setup)."
    }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $mt = $null
    if (Test-Path -LiteralPath $kits -PathType Container) {
        $versions = @(Get-ChildItem -LiteralPath $kits -Directory | Where-Object { $_.Name -match '^\d+(\.\d+){3}$' } |
            Sort-Object -Descending { [version]$_.Name })
        foreach ($v in $versions) {
            $candidate = Join-Path $v.FullName 'x64\mt.exe'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { $mt = $candidate; break }
        }
    }
    if ($null -eq $mt) {
        Fail "mt.exe isn't installed (Windows Kits\10\bin\<version>\x64). Install a Windows SDK with the Visual Studio Build Tools."
    }
    $tools = [pscustomobject]@{ Dumpbin = $dumpbin; Mt = $mt; CMake = $null; Cl = $null }
    if ($ForBuild) {
        $cmd = Get-Command cmake -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $cmd) {
            $tools.CMake = $cmd.Path
        } else {
            $tools.CMake = Find-VsFile $vswhere 'Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe' 'Microsoft.VisualStudio.Component.VC.CMake.Project'
        }
        if ($null -eq $tools.CMake) {
            Fail "CMake isn't on PATH or in Visual Studio. Install the Build Tools' C++ CMake tools (the C++ workload's recommended components)."
        }
        $tools.Cl = Find-VsFile $vswhere 'VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe' $msvc
        if ($null -eq $tools.Cl) { Fail "MSVC's x64 compiler (cl.exe) isn't installed." }
    }
    return $tools
}

# Windows' own tar, by its full path. Another OS has one only for testing the
# functions here.
function Get-Tar {
    if ($env:OS -eq 'Windows_NT') {
        if ([string]::IsNullOrEmpty($env:SystemRoot)) { Fail "SystemRoot isn't set, so Windows' tar.exe can't be found." }
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    } else {
        $tar = '/usr/bin/tar'
    }
    if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) { Fail "$tar is missing. Windows 10 1803 and later have it." }
    return $tar
}

function Test-Tarball([string]$Path, $Sources) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $false }
    if ((Get-Item -LiteralPath $Path).Length -ne $Sources.Size) { return $false }
    return (Get-Sha256 $Path) -ceq $Sources.Sha256
}

function Save-Tarball($Sources, [string]$Dl, [string]$Tarball) {
    if (Test-Tarball $Tarball $Sources) {
        Write-Host "Using dl/$([System.IO.Path]::GetFileName($Tarball)) (its size and SHA-256 match sources.json)."
        return
    }
    New-Item -ItemType Directory -Force -Path $Dl | Out-Null
    Remove-Item -LiteralPath $Tarball -Force -ErrorAction SilentlyContinue
    $script:dlTmp = Join-Path (Split-Path -Parent $Dl) ('dl-tmp-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $script:dlTmp | Out-Null
    $file = Join-Path $script:dlTmp ([System.IO.Path]::GetFileName($Tarball))
    Write-Host "Downloading libarchive $($Sources.Version) ($($Sources.Size) bytes) from $($Sources.Url)"
    $request = @{ Uri = $Sources.Url; OutFile = $file; UseBasicParsing = $true }
    if ($PSVersionTable.PSVersion.Major -ge 7) {
        # PowerShell 7 ignores ServicePointManager; it takes the versions here.
        $request['SslProtocol'] = 'Tls12, Tls13'
    }
    $attempt = 0
    while ($true) {
        $attempt++
        try {
            Invoke-WebRequest @request
            break
        } catch {
            Remove-Item -LiteralPath $file -Force -ErrorAction SilentlyContinue
            if ($attempt -ge 3) { Fail "couldn't download libarchive from $($Sources.Url) ($($_.Exception.Message))." }
            Write-Host "  The download failed ($($_.Exception.Message)). Trying again."
            Start-Sleep -Seconds 5
        }
    }
    $size = (Get-Item -LiteralPath $file).Length
    if ($size -ne $Sources.Size) {
        Fail "the download is $size bytes, but sources.json says $($Sources.Size). Deleted it; nothing was extracted."
    }
    $sha = Get-Sha256 $file
    if ($sha -cne $Sources.Sha256) {
        Fail "the download has SHA-256 $sha, but sources.json says $($Sources.Sha256). Deleted it; nothing was extracted."
    }
    Write-Host '  Its size and SHA-256 match sources.json.'
    Move-Item -LiteralPath $file -Destination $Tarball -Force
    Remove-Item -LiteralPath $script:dlTmp -Recurse -Force -ErrorAction SilentlyContinue
    $script:dlTmp = $null
}

function Expand-Tarball([string]$Tarball, [string]$Dest) {
    New-Item -ItemType Directory -Force -Path $Dest | Out-Null
    $r = Invoke-Captured (Get-Tar) @('-xzf', $Tarball, '-C', $Dest)
    if ($r.Code -ne 0) {
        foreach ($line in $r.Lines) { [Console]::Error.WriteLine($line) }
        Fail "tar couldn't extract $([System.IO.Path]::GetFileName($Tarball)) (exit code $($r.Code))."
    }
}

function Get-HashOrNone([string]$Path) {
    if (Test-Path -LiteralPath $Path -PathType Leaf) { return Get-Sha256 $Path }
    return 'none'
}

# Applies the patches in order, as build.sh does: inside the Grape checkout
# (target/ is in it), git apply reads a patch's paths from the checkout's top,
# skips every file and still exits 0. A throwaway repository in the extract
# is the nearest one, so the paths are read from there, and every file a
# patch names must have changed afterwards.
function Invoke-Patches([string]$Src, $Sources, [string]$PatchDir) {
    foreach ($name in 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE') {
        Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
    }
    $git = (Get-Command git -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1)
    if ($null -eq $git) { Fail 'git is missing. Install it (docs/PLATFORM_WINDOWS.md, Dev setup).' }
    $git = $git.Path
    Push-Location -LiteralPath $Src
    try {
        $r = Invoke-Captured $git @('-c', 'init.defaultBranch=main', 'init', '-q')
        if ($r.Code -ne 0) { Fail "git couldn't make a scratch repository in the extract." }
        foreach ($p in $Sources.Patches) {
            $patch = Join-Path $PatchDir $p.File
            if (-not (Test-Path -LiteralPath $patch -PathType Leaf)) { Fail "scripts/bsdtar/patches/$($p.File) is missing." }
            $sha = Get-Sha256 $patch
            if ($sha -cne $p.Sha256) { Fail "patches/$($p.File) has SHA-256 $sha, but sources.json says $($p.Sha256)." }
            $r = Invoke-Captured $git @('apply', '--numstat', $patch)
            if ($r.Code -ne 0) { Fail "git can't read patches/$($p.File)." }
            # "<added>\t<deleted>\t<path>" lines only; stderr is mixed in.
            $touched = @($r.Lines | Where-Object { $_ -match '^(\d+|-)\t(\d+|-)\t.' } | ForEach-Object { ($_ -split "`t", 3)[2] })
            if ($touched.Count -eq 0) { Fail "patches/$($p.File) changes no file." }
            $before = @{}
            foreach ($t in $touched) { $before[$t] = Get-HashOrNone (Join-Path $Src $t) }
            $r = Invoke-Captured $git @('-c', 'core.autocrlf=false', 'apply', '--check', $patch)
            if ($r.Code -ne 0) {
                foreach ($line in $r.Lines) { [Console]::Error.WriteLine($line) }
                Fail "patches/$($p.File) doesn't apply to libarchive $($Sources.Version)."
            }
            $r = Invoke-Captured $git @('-c', 'core.autocrlf=false', 'apply', $patch)
            if ($r.Code -ne 0) {
                foreach ($line in $r.Lines) { [Console]::Error.WriteLine($line) }
                Fail "patches/$($p.File) didn't apply."
            }
            # Every file it names must have changed: git skips files silently.
            foreach ($t in $touched) {
                if ((Get-HashOrNone (Join-Path $Src $t)) -ceq $before[$t]) {
                    Fail "patches/$($p.File) didn't change $t (git skipped it)."
                }
            }
            Write-Host "Applied $($p.File)."
        }
    } finally {
        Pop-Location
    }
}

# The files a CMakeLists.txt lists in a variable: its SET(...) block plus
# every LIST(APPEND ...), relative to the source tree.
function Get-CMakeListed([string]$Src, [string]$CMake, [string]$Var, [string]$Base) {
    $path = Join-Path $Src $CMake
    try {
        $text = Read-Utf8 $path
    } catch {
        Fail "copyright lines: can't read $CMake ($($_.Exception.GetBaseException().Message))."
    }
    $options = [System.Text.RegularExpressions.RegexOptions]'IgnoreCase, CultureInvariant'
    $m = [regex]::Match($text, "SET\($Var[ \t\r\n]([^)]*)\)", $options)
    if (-not $m.Success) { Fail "copyright lines: $CMake has no SET($Var ...)." }
    $words = New-Object System.Collections.Generic.List[string]
    foreach ($line in [regex]::Split($m.Groups[1].Value, "\r\n|\n|\r")) {
        foreach ($w in ($line.Split('#')[0] -split '\s+')) {
            if ($w.Length -gt 0) { $words.Add($w) }
        }
    }
    foreach ($mm in [regex]::Matches($text, "LIST\(APPEND[ \t]+$Var[ \t]+([^ \t\r\n)]+)[ \t]*\)", $options)) {
        $words.Add($mm.Groups[1].Value)
    }
    $files = @()
    foreach ($w in $words) {
        $parts = New-Object System.Collections.Generic.List[string]
        foreach ($part in "$Base/$w".Split('/')) {
            if ($part -eq '..') {
                if ($parts.Count -eq 0) { Fail "copyright lines: $CMake names a file outside the source tree: $w" }
                $parts.RemoveAt($parts.Count - 1)
            } elseif ($part -ne '' -and $part -ne '.') {
                $parts.Add($part)
            }
        }
        $files += ($parts -join '/')
    }
    return $files
}

# The copyright lines of the sources compiled into bsdtar, as
# licenses/libarchive/copyright-lines.txt has them. Exactly build.sh's
# copyright_lines: the files libarchive/CMakeLists.txt puts in
# include_HEADERS and libarchive_SOURCES (without archive_disk_acl_*: ACLs
# are off) and tar/CMakeLists.txt puts in bsdtar_SOURCES, .c and .h only. A
# line counts when, after any leading comment marker and spaces, it starts
# with "Copyright" or "Portions Copyright", then a space and "(c)", "(C)",
# the copyright sign (U+00A9) or a digit; its trailing spaces and "*/" are
# dropped. Distinct lines, sorted by code point, under a heading and a blank
# line. This file stays ASCII: Windows PowerShell 5.1 reads a script without a
# BOM in the ANSI code page, which would turn a literal copyright sign into
# two other characters, so the regex spells it \u00A9.
function Get-CopyrightLines([string]$Src) {
    $all = @(Get-CMakeListed $Src 'libarchive/CMakeLists.txt' 'include_HEADERS' 'libarchive')
    $all += @(Get-CMakeListed $Src 'libarchive/CMakeLists.txt' 'libarchive_SOURCES' 'libarchive')
    $all += @(Get-CMakeListed $Src 'tar/CMakeLists.txt' 'bsdtar_SOURCES' 'tar')
    $files = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    foreach ($f in $all) {
        $leaf = $f.Split('/')[-1]
        if (($f.EndsWith('.c', [System.StringComparison]::Ordinal) -or $f.EndsWith('.h', [System.StringComparison]::Ordinal)) -and
            -not $leaf.StartsWith('archive_disk_acl_', [System.StringComparison]::Ordinal)) {
            [void]$files.Add($f)
        }
    }
    if ($files.Count -lt 100) { Fail "copyright lines: only $($files.Count) source files listed; the CMake files must have changed shape." }
    $lead = New-Object regex '^[ \t]*(?:/\*+|\*+|//+|#+)?[ \t]*'
    $trail = New-Object regex '[ \t]*(?:\*/)?[ \t]*$'
    $copyright = New-Object regex '^(?:Portions )?Copyright (?:\([cC]\)|\u00A9|[0-9])'
    $found = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
    foreach ($f in $files) {
        try {
            $text = Read-Utf8 (Join-Path $Src $f)
        } catch {
            Fail "copyright lines: can't read $f as UTF-8 ($($_.Exception.GetBaseException().Message))."
        }
        foreach ($line in [regex]::Split($text, "\r\n|\n|\r")) {
            $s = $trail.Replace($lead.Replace($line, '', 1), '', 1)
            if ($copyright.IsMatch($s)) { [void]$found.Add($s) }
        }
    }
    if ($found.Count -eq 0) { Fail 'copyright lines: none found.' }
    $sorted = [string[]]@($found)
    [System.Array]::Sort($sorted, [System.StringComparer]::Ordinal)
    return $heading + "`n`n" + ($sorted -join "`n") + "`n"
}

function Test-Notices([string]$Src, [string]$Committed, [string]$RootDir) {
    $text = Get-CopyrightLines $Src
    $bytes = (New-Object System.Text.UTF8Encoding $false).GetBytes($text)
    $same = $false
    if (Test-Path -LiteralPath $Committed -PathType Leaf) {
        $have = [System.IO.File]::ReadAllBytes($Committed)
        $same = [System.Convert]::ToBase64String($have) -ceq [System.Convert]::ToBase64String($bytes)
    }
    if (-not $same) {
        $fresh = Join-Path $RootDir 'copyright-lines.txt'
        Write-Utf8 $fresh $text
        Fail "licenses/libarchive/copyright-lines.txt doesn't match the copyright lines of the sources this build compiled. The pin moved and the notices didn't: compare it with $fresh and copy that there (licenses/README.md)."
    }
    Write-Host '  licenses/libarchive/copyright-lines.txt matches the compiled sources.'
}

function Invoke-Build($Tools, [string]$Src, [string]$Build) {
    # Every switch explicit: the runner's Strawberry Perl carries zlib and
    # libxml2 on PATH, and CMake would otherwise take them silently.
    $configure = @(
        '-S', $Src, '-B', $Build, '-A', 'x64',
        '-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded', '-DMSVC_USE_STATIC_CRT=ON',
        '-DBUILD_SHARED_LIBS=OFF', '-DENABLE_TAR=ON', '-DENABLE_TAR_SHARED=OFF',
        '-DENABLE_CPIO=OFF', '-DENABLE_CAT=OFF', '-DENABLE_UNZIP=OFF',
        '-DENABLE_TEST=OFF', '-DENABLE_INSTALL=OFF', '-DENABLE_COVERAGE=OFF', '-DENABLE_WERROR=OFF',
        '-DENABLE_OPENSSL=OFF', '-DENABLE_MBEDTLS=OFF', '-DENABLE_NETTLE=OFF', '-DENABLE_CNG=OFF', '-DENABLE_MD=OFF',
        '-DENABLE_ZLIB=OFF', '-DENABLE_BZip2=OFF', '-DENABLE_LZMA=OFF', '-DENABLE_ZSTD=OFF',
        '-DENABLE_LZ4=OFF', '-DENABLE_LZO=OFF', '-DENABLE_LIBB2=OFF',
        '-DENABLE_LIBXML2=OFF', '-DENABLE_BSDXML=OFF', '-DENABLE_EXPAT=OFF', '-DENABLE_WIN32_XMLLITE=OFF',
        '-DENABLE_PCREPOSIX=OFF', '-DENABLE_PCRE2POSIX=OFF', '-DPOSIX_REGEX_LIB=NONE', '-DENABLE_LIBGCC=OFF',
        '-DENABLE_ICONV=OFF', '-DENABLE_ACL=OFF', '-DENABLE_XATTR=OFF',
        '-DWINDOWS_VERSION=WIN10'
    )
    Write-Host 'Configuring bsdtar for Windows x64...'
    Invoke-Streamed $Tools.CMake $configure 'CMake configure'
    Write-Host 'Building bsdtar...'
    Invoke-Streamed $Tools.CMake @('--build', $Build, '--config', 'Release', '--target', 'bsdtar') 'The build'
    $exe = Join-Path $Build 'bin\Release\bsdtar.exe'
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Fail "the build has no bin\Release\bsdtar.exe." }
    return $exe
}

function Write-BuildInfo($Tools, $Sources, [string]$SourcesPath, [string]$Build, [string]$Out) {
    $cmakeLine = (Invoke-Captured $Tools.CMake @('--version')).Lines | Select-Object -First 1
    # cl prints its banner on stderr when run without arguments.
    $clLine = @((Invoke-Captured $Tools.Cl @()).Lines | Where-Object { $_ -match 'Version' }) | Select-Object -First 1
    $generator = 'unknown'
    $cache = Join-Path $Build 'CMakeCache.txt'
    if (Test-Path -LiteralPath $cache) {
        $g = Select-String -LiteralPath $cache -Pattern '^CMAKE_GENERATOR:INTERNAL=(.*)$' | Select-Object -First 1
        if ($null -ne $g) { $generator = $g.Matches[0].Groups[1].Value }
    }
    $sdk = 'unknown'
    $proj = Join-Path $Build 'tar\bsdtar.vcxproj'
    if (Test-Path -LiteralPath $proj) {
        $m = Select-String -LiteralPath $proj -Pattern '<WindowsTargetPlatformVersion>([^<]+)<' | Select-Object -First 1
        if ($null -ne $m) { $sdk = $m.Matches[0].Groups[1].Value }
    }
    $text = "Grape's bsdtar (DECISIONS D27), built by scripts/bsdtar/build.ps1`n"
    $text += "sources.json SHA-256: $(Get-Sha256 $SourcesPath)`n"
    $text += "marker: $($Sources.Marker)`n"
    $text += "libarchive: $($Sources.Version), libarchive-$($Sources.Version).tar.gz SHA-256 $($Sources.Sha256)`n"
    $text += "patches:`n"
    foreach ($p in $Sources.Patches) { $text += "  $($p.File) $($p.Sha256)`n" }
    $text += "cmake: $cmakeLine`n"
    $text += "generator: $generator`n"
    $text += "compiler: $clLine`n"
    $text += "sdk: Windows SDK $sdk`n"
    $text += "os: $([System.Environment]::OSVersion.VersionString), $env:PROCESSOR_ARCHITECTURE`n"
    $text += "built: $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))`n"
    Write-Utf8 (Join-Path $Out 'BUILDINFO.txt') $text
    Write-Utf8 (Join-Path $Out 'SHA256SUMS') "$(Get-Sha256 (Join-Path $Out $exeName))  $exeName`n"
}

# The first line of --version must be sources.json's exactly, and every token
# after "libarchive <version>" must be an allowed one.
function Test-VersionLine([string]$Exe, $Sources) {
    $r = Invoke-Captured $Exe @('--version')
    if ($r.Code -ne 0 -or $r.Lines.Count -eq 0) { Fail "$exeName didn't run with --version (exit code $($r.Code))." }
    $first = $r.Lines[0]
    if ($first -cne $Sources.VersionLine) {
        Fail "$exeName's version line is '$first', but sources.json wants '$($Sources.VersionLine)' (trailing space included)."
    }
    $prefix = "bsdtar $($Sources.Version)-$($Sources.Marker) - libarchive $($Sources.Version)"
    if (-not $first.StartsWith($prefix, [System.StringComparison]::Ordinal)) {
        Fail "$exeName's version line doesn't start with '$prefix'."
    }
    foreach ($tok in ($first.Substring($prefix.Length).Split(' ') | Where-Object { $_ })) {
        if ($allowedTokens -cnotcontains $tok) { Fail "$exeName's version line has '$tok': a library got into the build." }
    }
    Write-Host "  ${exeName}: $first"
}

function Test-Output($Tools, $Sources, [string]$Out) {
    $exe = Join-Path $Out $exeName
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Fail "out/$exeName is missing. Build it first (without -CheckOnly)." }
    $sums = Join-Path $Out 'SHA256SUMS'
    if (-not (Test-Path -LiteralPath $sums -PathType Leaf)) { Fail 'out/SHA256SUMS is missing.' }
    $want = $null
    foreach ($line in [System.IO.File]::ReadAllLines($sums)) {
        if ($line -cmatch '^([0-9a-f]{64})  (.+)$' -and $Matches[2] -ceq $exeName) { $want = $Matches[1] }
    }
    if ($want -cne (Get-Sha256 $exe)) { Fail "out/$exeName doesn't match out/SHA256SUMS." }
    if (-not (Test-Path -LiteralPath (Join-Path $Out 'BUILDINFO.txt') -PathType Leaf)) { Fail 'out/BUILDINFO.txt is missing.' }

    # Imports: a subset of sources.json's windows_dlls, ignoring case.
    $r = Invoke-Captured $Tools.Dumpbin @('/nologo', '/dependents', $exe)
    if ($r.Code -ne 0) { Fail "dumpbin couldn't read $exeName." }
    $dlls = @($r.Lines | Where-Object { $_ -match '^\s+(\S+\.dll)\s*$' } | ForEach-Object { $_.Trim() })
    if ($dlls.Count -eq 0) { Fail "dumpbin lists no DLL for $exeName; it can't have read it." }
    foreach ($dll in $dlls) {
        $ok = $false
        foreach ($allowed in $Sources.Dlls) { if ($dll -ieq $allowed) { $ok = $true } }
        if (-not $ok) { Fail "$exeName imports $dll, which sources.json's windows_dlls doesn't allow." }
    }
    Write-Host "  Imports only $($dlls -join ', ')."

    $r = Invoke-Captured $Tools.Dumpbin @('/nologo', '/headers', $exe)
    if ($r.Code -ne 0 -or -not ($r.Lines -match '8664 machine \(x64\)')) { Fail "$exeName isn't an x64 program." }
    Write-Host '  x64.'

    # The embedded manifest: activeCodePage UTF-8 (patch 0005) and longPathAware.
    $xml = Join-Path ([System.IO.Path]::GetTempPath()) ('grape-bsdtar-manifest-' + [guid]::NewGuid().ToString('N') + '.xml')
    try {
        $r = Invoke-Captured $Tools.Mt @('-nologo', "-inputresource:$exe;#1", "-out:$xml")
        if ($r.Code -ne 0 -or -not (Test-Path -LiteralPath $xml -PathType Leaf)) { Fail "mt.exe couldn't read $exeName's manifest." }
        $doc = New-Object System.Xml.XmlDocument
        $doc.Load($xml)
        $acp = @($doc.SelectNodes("//*[local-name()='activeCodePage']") | ForEach-Object { $_.InnerText.Trim() })
        $lpa = @($doc.SelectNodes("//*[local-name()='longPathAware']") | ForEach-Object { $_.InnerText.Trim() })
        if ($acp -cnotcontains 'UTF-8') { Fail "$exeName's manifest has no activeCodePage UTF-8 (patch 0005)." }
        if ($lpa -notcontains 'true') { Fail "$exeName's manifest isn't longPathAware." }
    } finally {
        Remove-Item -LiteralPath $xml -Force -ErrorAction SilentlyContinue
    }
    Write-Host '  Manifest: activeCodePage UTF-8, longPathAware.'

    Test-VersionLine $exe $Sources
}

function Invoke-Main {
    if ($env:OS -ne 'Windows_NT') {
        Fail 'this script is for Windows. On macOS, run ./scripts/bsdtar/build.sh.'
    }
    $scriptDir = $PSScriptRoot
    $repo = Split-Path -Parent (Split-Path -Parent $scriptDir)
    $sourcesPath = Join-Path $scriptDir 'sources.json'
    if (-not (Test-Path -LiteralPath $sourcesPath -PathType Leaf)) { Fail "can't find scripts/bsdtar/sources.json." }
    $sources = Read-Sources $sourcesPath

    if ([string]::IsNullOrEmpty($Root)) {
        $rootDir = Join-Path $repo 'target\bsdtar'
    } elseif ([System.IO.Path]::IsPathRooted($Root)) {
        $rootDir = $Root
    } else {
        $rootDir = Join-Path (Get-Location).ProviderPath $Root
    }
    $rootDir = [System.IO.Path]::GetFullPath($rootDir)
    New-Item -ItemType Directory -Force -Path $rootDir | Out-Null
    $dl = Join-Path $rootDir 'dl'
    $script:work = Join-Path $rootDir 'work'
    $out = Join-Path $rootDir 'out'
    $tarball = Join-Path $dl "libarchive-$($sources.Version).tar.gz"

    $tools = Get-Tools (-not $CheckOnly)
    if (-not $CheckOnly) {
        Save-Tarball $sources $dl $tarball
        if (Test-Path -LiteralPath $script:work) { Remove-Item -LiteralPath $script:work -Recurse -Force }
        $srcRoot = Join-Path $script:work 'src'
        Expand-Tarball $tarball $srcRoot
        $src = Join-Path $srcRoot "libarchive-$($sources.Version)"
        if (-not (Test-Path -LiteralPath $src -PathType Container)) { Fail "the tarball has no libarchive-$($sources.Version) folder." }
        Invoke-Patches $src $sources (Join-Path $scriptDir 'patches')
        $built = Invoke-Build $tools $src (Join-Path $script:work 'build')
        if (Test-Path -LiteralPath $out) { Remove-Item -LiteralPath $out -Recurse -Force }
        New-Item -ItemType Directory -Path $out | Out-Null
        Copy-Item -LiteralPath $built -Destination (Join-Path $out $exeName)
        Write-BuildInfo $tools $sources $sourcesPath (Join-Path $script:work 'build') $out
        Write-Host 'Checking the result...'
        Test-Output $tools $sources $out
        Test-Notices $src (Join-Path $repo 'licenses\libarchive\copyright-lines.txt') $rootDir
    } else {
        Write-Host "Checking what $out holds..."
        Test-Output $tools $sources $out
    }

    if ($Install) {
        $binDir = Join-Path $repo 'apps\desktop\src-tauri\binaries'
        New-Item -ItemType Directory -Force -Path $binDir | Out-Null
        $tmp = Join-Path $binDir "$exeName.tmp"
        Copy-Item -LiteralPath (Join-Path $out $exeName) -Destination $tmp -Force
        Move-Item -LiteralPath $tmp -Destination (Join-Path $binDir $exeName) -Force
        Write-Host "Installed $exeName into apps\desktop\src-tauri\binaries."
    }
    $length = (Get-Item -LiteralPath (Join-Path $out $exeName)).Length
    Write-Host "bsdtar is ready in ${out}:"
    Write-Host ('  {0,-36} {1,10} bytes' -f $exeName, $length)
}

# Dot-sourcing the script (. .\build.ps1) only defines the functions, so
# they can be tried on their own.
if ($MyInvocation.InvocationName -ne '.') {
    $script:work = $null
    $script:dlTmp = $null
    $tls = [System.Net.ServicePointManager]::SecurityProtocol
    $exitCode = 0
    try {
        # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
        [System.Net.ServicePointManager]::SecurityProtocol = $tls -bor [System.Net.SecurityProtocolType]::Tls12
        Invoke-Main
    } catch {
        [Console]::Error.WriteLine("${me}: $($_.Exception.Message)")
        $exitCode = 1
    } finally {
        [System.Net.ServicePointManager]::SecurityProtocol = $tls
        foreach ($dir in $script:work, $script:dlTmp) {
            if ($null -ne $dir -and (Test-Path -LiteralPath $dir)) {
                Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
            }
        }
    }
    exit $exitCode
}
