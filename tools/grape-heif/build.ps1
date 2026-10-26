<#
.SYNOPSIS
Builds grape-heif.exe, Grape's HEIC helper for Windows (DECISIONS D33).

.DESCRIPTION
grape-heif.exe is a small Rust program (this folder) on libheif-rs, linking
libheif, libde265 and x265 statically. vcpkg builds those C libraries; this
script pins and checks everything in vcpkg.lock.json. CI's grape-heif
workflow and a PC run the same steps (heic-spec J13):

  1. reads vcpkg.lock.json;
  2. clones vcpkg into -VcpkgRoot at the pinned commit (git init, fetch
     --depth 1, checkout --detach) and checks HEAD; an existing clone is
     reused only if it has no local changes. Never the runner's C:\vcpkg;
  3. bootstraps it (bootstrap-vcpkg.bat -disableMetrics; the vcpkg tool's
     version is pinned by that commit);
  4. sources first: `vcpkg install libheif[core,hevc]:x64-windows-static
     --only-downloads --binarysource=clear` with the overlay port in
     vcpkg/ports, then checks that every archive in the lock is in
     <root>\downloads by its SHA-512, that each SHA-512 is the one the port
     (or the overlay) checks, that each port's version is the lock's, and
     that x265's archive is of the pinned commit. Done first, so a binary
     cache hit can't leave the source bundle without its archives;
  5. builds: the same install, with a binary cache only if -BinaryCache is
     given (otherwise none: VCPKG_BINARY_SOURCES=clear), then checks that
     vcpkg installed the pinned versions;
  6. `cargo build --release --locked --bins` here, with VCPKG_ROOT and
     VCPKGRS_TRIPLET set and RUSTFLAGS removed (.cargo/config.toml turns on
     the static C runtime, and RUSTFLAGS would replace it);
  7. puts grape-heif.exe, BUILDINFO.txt (the banner, the pins, the tool
     versions) and SHA256SUMS in tools\grape-heif\target\out;
  8. with -Install, also copies it to
     apps\desktop\src-tauri\binaries\grape-heif-x86_64-pc-windows-msvc.exe,
     where `tauri build` with tauri.heif.windows.conf.json takes it.

CI checks the result further (the banner, no plugin loader, the DLLs it
imports, the license texts): .github/workflows/grape-heif.yml.

It needs git, Rust 1.99 and the Visual Studio Build Tools with the C++
workload (docs/PLATFORM_WINDOWS.md, Dev setup); vcpkg fetches CMake, NASM
and pkgconf itself. The first build takes 30 minutes or more and about 3 GB.
It never changes PATH and installs nothing outside -VcpkgRoot, this folder's
target\ and, with -Install, binaries\. Works in Windows PowerShell 5.1 and
PowerShell 7.

.PARAMETER VcpkgRoot
An empty folder for vcpkg, or one this script cloned before. Default:
target\grape-heif\vcpkg in the repo. Keep the path short: vcpkg's build
trees are deep.

.PARAMETER BinaryCache
A folder for vcpkg's binary cache (read and written). Without it, nothing is
cached and everything is built.

.PARAMETER Install
Also copy the result into apps\desktop\src-tauri\binaries\.

.EXAMPLE
.\tools\grape-heif\build.ps1 -VcpkgRoot C:\gh-vcpkg -BinaryCache C:\gh-vcpkg-cache -Install
#>
[CmdletBinding()]
param(
    [string]$VcpkgRoot,
    [string]$BinaryCache,
    [switch]$Install
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$me = 'grape-heif build'
$triple = 'x86_64-pc-windows-msvc'

function Fail([string]$Message) {
    throw $Message
}

function Get-Sha([string]$Algorithm, [string]$Path) {
    return (Get-FileHash -Algorithm $Algorithm -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Write-Utf8([string]$Path, [string]$Text) {
    # LF and no BOM.
    [System.IO.File]::WriteAllText($Path, $Text, (New-Object System.Text.UTF8Encoding $false))
}

# The value of one key of a JSON object, or $null (case-sensitive, as JSON).
function Get-Key($Obj, [string]$Name) {
    if ($null -eq $Obj) { return $null }
    foreach ($p in $Obj.PSObject.Properties) {
        if ($p.Name -ceq $Name) { return $p.Value }
    }
    return $null
}

function Get-String($Obj, [string]$Name, [string]$Where, [string]$Pattern) {
    $v = Get-Key $Obj $Name
    if ($v -isnot [string] -or $v -cnotmatch $Pattern) {
        Fail "tools/grape-heif/vcpkg.lock.json: $Where.$Name is missing or malformed."
    }
    return $v
}

# Reads and checks vcpkg.lock.json.
function Read-Lock([string]$Path) {
    try {
        $j = [System.IO.File]::ReadAllText($Path) | ConvertFrom-Json
    } catch {
        Fail "tools/grape-heif/vcpkg.lock.json isn't valid JSON ($($_.Exception.Message))."
    }
    $v = Get-Key $j 'vcpkg'
    $lock = [pscustomobject]@{
        Repo    = Get-String $v 'repo' 'vcpkg' '^https://github\.com/microsoft/vcpkg\.git$'
        Commit  = Get-String $v 'commit' 'vcpkg' '^[0-9a-f]{40}$'
        Triplet = Get-String $j 'triplet' '(root)' '^[a-z0-9-]+$'
        Install = Get-String $j 'install' '(root)' '^[a-z0-9-]+\[[a-z0-9,-]+\]$'
        Sources = @()
    }
    $sources = @(Get-Key $j 'sources')
    if ($sources.Count -eq 0) { Fail 'tools/grape-heif/vcpkg.lock.json lists no sources.' }
    foreach ($s in $sources) {
        $port = Get-String $s 'port' 'sources[]' '^[a-z0-9-]+$'
        $entry = [pscustomobject]@{
            Port    = $port
            Version = Get-String $s 'version' "sources[$port]" '^[0-9][0-9.]*$'
            Url     = Get-String $s 'url' "sources[$port]" '^https://'
            Sha512  = Get-String $s 'sha512' "sources[$port]" '^[0-9a-f]{128}$'
            Sha256  = Get-Key $s 'sha256'
            Size    = Get-Key $s 'size'
            Commit  = Get-Key $s 'commit'
        }
        if ($null -ne $entry.Sha256 -and ($entry.Sha256 -isnot [string] -or $entry.Sha256 -cnotmatch '^[0-9a-f]{64}$')) {
            Fail "tools/grape-heif/vcpkg.lock.json: sources[$port].sha256 is malformed."
        }
        if ($null -ne $entry.Commit -and ($entry.Commit -isnot [string] -or $entry.Commit -cnotmatch '^[0-9a-f]{40}$')) {
            Fail "tools/grape-heif/vcpkg.lock.json: sources[$port].commit is malformed."
        }
        $lock.Sources += $entry
    }
    return $lock
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

# Runs a program with its output going to the console.
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

function Invoke-Git([string]$Root, [string[]]$Arguments) {
    $r = Invoke-Captured 'git' (@('-C', $Root) + $Arguments)
    if ($r.Code -ne 0) { Fail "git $($Arguments -join ' ') failed: $($r.Lines -join ' ')" }
    return $r.Lines
}

function Resolve-Folder([string]$Given, [string]$Default) {
    if ([string]::IsNullOrEmpty($Given)) { return [System.IO.Path]::GetFullPath($Default) }
    if ([System.IO.Path]::IsPathRooted($Given)) { return [System.IO.Path]::GetFullPath($Given) }
    return [System.IO.Path]::GetFullPath((Join-Path (Get-Location).ProviderPath $Given))
}

# Steps 2 and 3: vcpkg at the pinned commit, bootstrapped.
function Get-Vcpkg($Lock, [string]$Root) {
    $exists = Test-Path -LiteralPath $Root -PathType Container
    $empty = (-not $exists) -or (@(Get-ChildItem -LiteralPath $Root -Force).Count -eq 0)
    if ($empty) {
        New-Item -ItemType Directory -Force -Path $Root | Out-Null
        Invoke-Git $Root @('init', '-q') | Out-Null
    } elseif (-not (Test-Path -LiteralPath (Join-Path $Root '.git'))) {
        Fail "$Root isn't empty and isn't a vcpkg clone this script made. Pass an empty folder as -VcpkgRoot."
    } else {
        # vcpkg's .gitignore covers downloads, buildtrees, packages and
        # installed, so a clean status means no port was edited.
        $changed = @(Invoke-Git $Root @('status', '--porcelain', '--untracked-files=no'))
        if ($changed.Count -ne 0) { Fail "the vcpkg clone in $Root has local changes; use a new folder." }
    }
    $r = Invoke-Captured 'git' @('-C', $Root, 'rev-parse', '--verify', '-q', 'HEAD')
    $head = @($r.Lines)
    if ($r.Code -ne 0 -or $head.Count -eq 0 -or $head[0] -cne $Lock.Commit) {
        Write-Host "Fetching vcpkg $($Lock.Commit)..."
        Invoke-Git $Root @('fetch', '-q', '--depth', '1', $Lock.Repo, $Lock.Commit) | Out-Null
        Invoke-Git $Root @('checkout', '-q', '--detach', 'FETCH_HEAD') | Out-Null
    }
    $head = @(Invoke-Git $Root @('rev-parse', 'HEAD'))[0]
    if ($head -cne $Lock.Commit) { Fail "vcpkg's HEAD is $head, not the pinned $($Lock.Commit)." }
    Write-Host "vcpkg is at $head."

    $exe = Join-Path $Root 'vcpkg.exe'
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        Invoke-Streamed (Join-Path $Root 'bootstrap-vcpkg.bat') @('-disableMetrics') 'bootstrap-vcpkg'
    }
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { Fail 'bootstrap-vcpkg made no vcpkg.exe.' }
    return $exe
}

function Invoke-Vcpkg([string]$Exe, [string]$Root, [string[]]$Arguments, [string]$What) {
    # Classic mode: the vcpkg root has no vcpkg.json, and neither does any
    # folder above it.
    Push-Location -LiteralPath $Root
    try {
        Invoke-Streamed $Exe (@("--vcpkg-root=$Root") + $Arguments) $What
    } finally {
        Pop-Location
    }
}

# The one SHA512 a portfile checks.
function Get-PortSha512([string]$Portfile) {
    $text = [System.IO.File]::ReadAllText($Portfile)
    $found = @([regex]::Matches($text, 'SHA512\s+([0-9a-fA-F]{128})') | ForEach-Object { $_.Groups[1].Value.ToLowerInvariant() })
    if ($found.Count -ne 1) { Fail "$Portfile checks $($found.Count) SHA512s, not one." }
    return $found[0]
}

function Get-PortVersion([string]$Manifest) {
    $j = [System.IO.File]::ReadAllText($Manifest) | ConvertFrom-Json
    foreach ($key in 'version', 'version-semver', 'version-string', 'version-date') {
        $v = Get-Key $j $key
        if ($null -ne $v) { return [string]$v }
    }
    Fail "$Manifest has no version."
}

# The commit GitHub wrote into an archive's pax header (what
# `git get-tar-commit-id` reads), or $null.
function Get-TarCommit([string]$Path) {
    $file = [System.IO.File]::OpenRead($Path)
    try {
        $gz = New-Object System.IO.Compression.GZipStream $file, ([System.IO.Compression.CompressionMode]::Decompress)
        $buffer = New-Object byte[] 1536
        $read = 0
        while ($read -lt $buffer.Length) {
            $n = $gz.Read($buffer, $read, $buffer.Length - $read)
            if ($n -le 0) { break }
            $read += $n
        }
    } finally {
        $file.Dispose()
    }
    $text = [System.Text.Encoding]::ASCII.GetString($buffer, 0, $read)
    $m = [regex]::Match($text, '\d+ comment=([0-9a-f]{40})\n')
    if ($m.Success) { return $m.Groups[1].Value }
    return $null
}

# Step 4's checks: every pinned archive is downloaded, is the one its port
# checks, and is the version the lock names.
function Test-Sources($Lock, [string]$Root, [string]$Overlay) {
    $downloads = Join-Path $Root 'downloads'
    if (-not (Test-Path -LiteralPath $downloads -PathType Container)) { Fail 'vcpkg downloaded nothing.' }
    $byHash = @{}
    foreach ($f in @(Get-ChildItem -LiteralPath $downloads -File)) {
        $byHash[(Get-Sha 'SHA512' $f.FullName)] = $f.FullName
    }
    foreach ($s in $Lock.Sources) {
        $overlayDir = Join-Path $Overlay $s.Port
        if (Test-Path -LiteralPath $overlayDir -PathType Container) { $portDir = $overlayDir } else { $portDir = Join-Path $Root "ports\$($s.Port)" }
        $portSha = Get-PortSha512 (Join-Path $portDir 'portfile.cmake')
        if ($portSha -cne $s.Sha512) { Fail "$($s.Port): the lock's SHA-512 isn't the one $portDir\portfile.cmake checks." }
        $portVersion = Get-PortVersion (Join-Path $portDir 'vcpkg.json')
        if ($portVersion -cne $s.Version) { Fail "$($s.Port): the port is version $portVersion, the lock says $($s.Version)." }
        $file = $byHash[$s.Sha512]
        if ($null -eq $file) { Fail "$($s.Port): no file in $downloads has the lock's SHA-512." }
        if ($null -ne $s.Sha256 -and (Get-Sha 'SHA256' $file) -cne $s.Sha256) { Fail "$($s.Port): $file has the right SHA-512 but not the lock's SHA-256." }
        if ($null -ne $s.Size -and (Get-Item -LiteralPath $file).Length -ne [long]$s.Size) { Fail "$($s.Port): $file isn't $($s.Size) bytes." }
        if ($null -ne $s.Commit) {
            $commit = Get-TarCommit $file
            if ($commit -cne $s.Commit) { Fail "$($s.Port): the archive is of commit $commit, not the pinned $($s.Commit)." }
        }
        Write-Host ("  {0,-9} {1,-7} {2}" -f $s.Port, $s.Version, [System.IO.Path]::GetFileName($file))
    }
}

# Step 5's check: vcpkg installed the pinned versions, for the triplet.
function Test-Installed([string]$Exe, [string]$Root, $Lock) {
    $r = Invoke-Captured $Exe @("--vcpkg-root=$Root", 'list')
    if ($r.Code -ne 0) { Fail 'vcpkg list failed.' }
    foreach ($s in $Lock.Sources) {
        $pattern = '^' + [regex]::Escape("$($s.Port):$($Lock.Triplet)") + '\s+' + [regex]::Escape($s.Version) + '(#\d+)?\s'
        if (-not ($r.Lines | Where-Object { $_ -cmatch $pattern })) {
            Fail "vcpkg didn't install $($s.Port) $($s.Version) for $($Lock.Triplet)."
        }
    }
    $installed = Join-Path $Root "installed\$($Lock.Triplet)"
    foreach ($f in 'lib\heif.lib', 'share\libheif\copyright', 'share\libde265\copyright', 'share\x265\copyright') {
        if (-not (Test-Path -LiteralPath (Join-Path $installed $f) -PathType Leaf)) { Fail "$installed\$f is missing." }
    }
    return @($r.Lines | Where-Object { $_ -match ':' + [regex]::Escape($Lock.Triplet) + '\s' })
}

# The first line of a program's output that matches, or "unknown".
function Get-VersionLine([string]$Exe, [string[]]$Arguments, [string]$Pattern) {
    if ([string]::IsNullOrEmpty($Exe)) { return 'unknown' }
    $r = Invoke-Captured $Exe $Arguments
    $line = $r.Lines | Where-Object { $_ -match $Pattern } | Select-Object -First 1
    if ($null -eq $line) { return 'unknown' }
    return $line.Trim()
}

# The tool vcpkg itself uses (`vcpkg fetch`), or $null.
function Get-VcpkgTool([string]$Exe, [string]$Root, [string]$Name) {
    $r = Invoke-Captured $Exe @("--vcpkg-root=$Root", 'fetch', $Name)
    if ($r.Code -ne 0) { return $null }
    $path = $r.Lines | Where-Object { $_ -and (Test-Path -LiteralPath $_.Trim() -PathType Leaf) } | Select-Object -Last 1
    if ($null -eq $path) { return $null }
    return $path.Trim()
}

function Get-Cl {
    $base = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrEmpty($base)) { return $null }
    $vswhere = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { return $null }
    $r = Invoke-Captured $vswhere @('-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-find', 'VC\Tools\MSVC\*\bin\Hostx64\x64\cl.exe')
    $found = @($r.Lines | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) })
    if ($found.Count -eq 0) { return $null }
    return $found[$found.Count - 1]
}

# The Visual Studio whose C++ tools built this, as vswhere names it
# ("Visual Studio Enterprise 2026 18.11.12315.225"), or 'unknown'. vcpkg and
# cargo both take the newest one with the C++ tools, which vswhere -latest
# finds too.
function Get-VsLine {
    $base = ${env:ProgramFiles(x86)}
    if ([string]::IsNullOrEmpty($base)) { return 'unknown' }
    $vswhere = Join-Path $base 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { return 'unknown' }
    $ask = @('-latest', '-products', '*', '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64', '-property')
    $name = Invoke-Captured $vswhere ($ask + 'displayName')
    $version = Invoke-Captured $vswhere ($ask + 'installationVersion')
    if ($name.Code -ne 0 -or $version.Code -ne 0 -or @($name.Lines).Count -lt 1 -or @($version.Lines).Count -lt 1) { return 'unknown' }
    return "$(@($name.Lines)[0].Trim()) $(@($version.Lines)[0].Trim())"
}

function Write-BuildInfo($Lock, [string]$Root, [string]$Vcpkg, [string[]]$Ports, [string]$Repo, [string]$Out) {
    $banner = Invoke-Captured (Join-Path $Out 'grape-heif.exe') @('-hide_banner', '-version')
    if ($banner.Code -ne 0 -or @($banner.Lines).Count -lt 1 -or $banner.Lines[0] -cnotmatch '^grape-heif version \S+-grape\.\d+ ') {
        Fail "grape-heif.exe's banner isn't right: $($banner.Lines -join ' / ')"
    }
    $commit = Invoke-Captured 'git' @('-C', $Repo, 'rev-parse', 'HEAD')
    $commitLine = 'unknown'
    if ($commit.Code -eq 0) { $commitLine = $commit.Lines[0] }
    $rustc = Invoke-Captured 'rustc' @('-vV')
    $cl = Get-Cl
    $info = New-Object System.Collections.Generic.List[string]
    $info.Add("grape-heif.exe, Grape's HEIC helper for Windows (DECISIONS D33)")
    $info.Add("SHA-256: $(Get-Sha 'SHA256' (Join-Path $Out 'grape-heif.exe'))")
    $info.Add("Grape commit: $commitLine")
    $info.Add("Built: $([DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'))")
    $info.Add('')
    $info.Add('Banner:')
    foreach ($l in $banner.Lines) { $info.Add("  $l") }
    $info.Add('')
    $info.Add("vcpkg: $($Lock.Repo) at $($Lock.Commit)")
    $info.Add("vcpkg tool: $(Get-VersionLine $Vcpkg @('--version') 'version')")
    $info.Add("Installed ($($Lock.Install), triplet $($Lock.Triplet)):")
    foreach ($p in $Ports) { $info.Add("  $($p.Trim())") }
    $info.Add('Sources (SHA-512):')
    foreach ($s in $Lock.Sources) { $info.Add("  $($s.Port) $($s.Version) $($s.Url) $($s.Sha512)") }
    $info.Add('')
    $info.Add('Tools:')
    foreach ($l in $rustc.Lines) { if ($l -match '^(rustc|host|release|commit-hash|LLVM)') { $info.Add("  $l") } }
    $info.Add("  cargo: $(Get-VersionLine 'cargo' @('-V') '^cargo ')")
    $info.Add("  Visual Studio: $(Get-VsLine)")
    $info.Add("  cl.exe: $(Get-VersionLine $cl @() 'Version')")
    $info.Add("  cmake: $(Get-VersionLine (Get-VcpkgTool $Vcpkg $Root 'cmake') @('--version') 'version')")
    $info.Add("  nasm: $(Get-VersionLine (Get-VcpkgTool $Vcpkg $Root 'nasm') @('-v') 'version')")
    Write-Utf8 (Join-Path $Out 'BUILDINFO.txt') (($info -join "`n") + "`n")
}

function Invoke-Main {
    if ($env:OS -ne 'Windows_NT') {
        Fail "this script is for Windows: grape-heif.exe is Grape's HEIC helper there (macOS uses ImageIO, D33)."
    }
    $toolDir = $PSScriptRoot
    $repo = Split-Path -Parent (Split-Path -Parent $toolDir)
    $lock = Read-Lock (Join-Path $toolDir 'vcpkg.lock.json')
    $root = Resolve-Folder $VcpkgRoot (Join-Path $repo 'target\grape-heif\vcpkg')
    $overlay = Join-Path $toolDir 'vcpkg\ports'
    $spec = "$($lock.Install):$($lock.Triplet)"

    Write-Host "== vcpkg at the pinned commit, in $root"
    $vcpkg = Get-Vcpkg $lock $root
    $env:VCPKG_DISABLE_METRICS = '1'

    Write-Host "== Sources ($spec)"
    $env:VCPKG_BINARY_SOURCES = 'clear'
    Invoke-Vcpkg $vcpkg $root @('install', $spec, "--overlay-ports=$overlay", '--only-downloads', '--binarysource=clear') 'vcpkg install --only-downloads'
    Test-Sources $lock $root $overlay

    Write-Host "== Build ($spec)"
    if ([string]::IsNullOrEmpty($BinaryCache)) {
        $env:VCPKG_BINARY_SOURCES = 'clear'
    } else {
        $cache = Resolve-Folder $BinaryCache $BinaryCache
        New-Item -ItemType Directory -Force -Path $cache | Out-Null
        $env:VCPKG_BINARY_SOURCES = "clear;files,$cache,readwrite"
    }
    Invoke-Vcpkg $vcpkg $root @('install', $spec, "--overlay-ports=$overlay", '--clean-buildtrees-after-build', '--clean-packages-after-build') 'vcpkg install'
    $ports = Test-Installed $vcpkg $root $lock

    Write-Host '== grape-heif.exe'
    foreach ($name in 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS', 'VCPKGRS_DYNAMIC') {
        if (Test-Path -LiteralPath "Env:$name") {
            Write-Host "  (ignoring $name for this build)"
            Remove-Item -LiteralPath "Env:$name"
        }
    }
    $env:VCPKG_ROOT = $root
    $env:VCPKGRS_TRIPLET = $lock.Triplet
    Push-Location -LiteralPath $toolDir
    try {
        Invoke-Streamed 'cargo' @('build', '--release', '--locked', '--bins') 'cargo build'
    } finally {
        Pop-Location
    }
    if ([string]::IsNullOrEmpty($env:CARGO_TARGET_DIR)) { $target = Join-Path $toolDir 'target' } else { $target = Resolve-Folder $env:CARGO_TARGET_DIR $env:CARGO_TARGET_DIR }
    $built = Join-Path $target 'release\grape-heif.exe'
    if (-not (Test-Path -LiteralPath $built -PathType Leaf)) { Fail "cargo made no $built." }

    $out = Join-Path $toolDir 'target\out'
    New-Item -ItemType Directory -Force -Path $out | Out-Null
    Copy-Item -LiteralPath $built -Destination (Join-Path $out 'grape-heif.exe') -Force
    Write-BuildInfo $lock $root $vcpkg $ports $repo $out
    $hash = Get-Sha 'SHA256' (Join-Path $out 'grape-heif.exe')
    Write-Utf8 (Join-Path $out 'SHA256SUMS') "$hash  grape-heif.exe`n"

    if ($Install) {
        $binDir = Join-Path $repo 'apps\desktop\src-tauri\binaries'
        New-Item -ItemType Directory -Force -Path $binDir | Out-Null
        $name = "grape-heif-$triple.exe"
        $tmp = Join-Path $binDir "$name.tmp"
        Copy-Item -LiteralPath (Join-Path $out 'grape-heif.exe') -Destination $tmp -Force
        Move-Item -LiteralPath $tmp -Destination (Join-Path $binDir $name) -Force
        Write-Host "Installed $name into apps\desktop\src-tauri\binaries."
    }
    Write-Host "grape-heif.exe is ready in ${out} (SHA-256 $hash)."
}

# Dot-sourcing the script (. .\build.ps1) only defines the functions.
if ($MyInvocation.InvocationName -ne '.') {
    $exitCode = 0
    try {
        Invoke-Main
    } catch {
        [Console]::Error.WriteLine("${me}: $($_.Exception.Message)")
        $exitCode = 1
    }
    exit $exitCode
}
