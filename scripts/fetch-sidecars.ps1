<#
.SYNOPSIS
Fetches the sidecars and libraries pinned in scripts/sidecars.lock for Windows
builds.

.DESCRIPTION
Puts FFmpeg and ffprobe in apps/desktop/src-tauri/binaries/ with Tauri's
target-triple names, ffmpeg-x86_64-pc-windows-msvc.exe and
ffprobe-x86_64-pc-windows-msvc.exe (DECISIONS D25; the pin itself is D22),
and PDFium as pdfium-x86_64-pc-windows-msvc.dll (D29).

It downloads each archive once into a new temp folder, checks its size and
SHA-256 before extracting anything, and extracts only the files the lock
names: from a zip with .NET, from a .tgz with Windows' own tar
(%SystemRoot%\System32\tar.exe, never one on PATH). The build's license text
is kept as binaries/ffmpeg-LICENSE.txt for the bundle step. Then it runs each
program with -hide_banner -version: the first line must name the lock's
version, and the configuration line must not have --enable-nonfree. A
library can't be run, so the SHA-256 of the file taken from its archive must
match the lock instead. The stamp file binaries/sidecars.stamp is written
last: the lock's hash, the target, and each file's SHA-256. A run that finds
them all matching changes nothing.

It never changes PATH and installs nothing. Works in Windows PowerShell 5.1
and PowerShell 7.

GRAPE_SIDECARS_NO_RUN=1 is for testing the download on another OS only. It
skips the -version runs and writes no stamp, so the next normal run checks
everything again.

.PARAMETER Force
Fetch again even when binaries/ already matches the lock.

.PARAMETER Only
Fetch just this one sidecar or library from the lock, such as pdfium. The
other files in binaries/ stay, and so do their lines in the stamp.

.EXAMPLE
.\scripts\fetch-sidecars.ps1

.EXAMPLE
.\scripts\fetch-sidecars.ps1 -Force

.EXAMPLE
.\scripts\fetch-sidecars.ps1 -Only pdfium
#>
[CmdletBinding()]
param(
    [switch]$Force,
    [string]$Only
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
# Windows PowerShell's progress bar makes big downloads many times slower.
$ProgressPreference = 'SilentlyContinue'

$me = 'fetch-sidecars'
$triple = 'x86_64-pc-windows-msvc'
$onlyGiven = $PSBoundParameters.ContainsKey('Only')

function Fail([string]$Message) {
    throw $Message
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# The value of one key of a JSON object, or $null. Keys are case-sensitive
# here, as in JSON, although PowerShell's objects aren't.
function Get-Key($Obj, [string]$Name) {
    foreach ($p in $Obj.PSObject.Properties) {
        if ($p.Name -ceq $Name) { return $p.Value }
    }
    return $null
}

# The keys of a JSON object, without the ones starting with _ (comments).
function Get-Entries($Obj, [string]$Where) {
    if ($Obj -isnot [System.Management.Automation.PSCustomObject]) {
        Fail "scripts/sidecars.lock: $Where must be an object."
    }
    return @($Obj.PSObject.Properties | Where-Object { -not $_.Name.StartsWith('_') })
}

function Test-Keys($Obj, [string]$Where, [string[]]$Required, [string[]]$Optional = @()) {
    foreach ($p in (Get-Entries $Obj $Where)) {
        if ($Required -cnotcontains $p.Name -and $Optional -cnotcontains $p.Name) {
            Fail "scripts/sidecars.lock: $Where has an unknown key '$($p.Name)'."
        }
    }
    foreach ($key in $Required) {
        if ($null -eq (Get-Key $Obj $key)) {
            Fail "scripts/sidecars.lock: $Where is missing '$key'."
        }
    }
}

# A plain relative path: no wildcards, no backslashes, nothing absolute, no
# leading - (tar and unzip would read it as an option) and no . or .. parts.
# The same rules as in fetch-sidecars.sh.
function Test-ArchivePath($Path, [string]$Where) {
    if ($Path -isnot [string] -or $Path.Length -eq 0) {
        Fail "scripts/sidecars.lock: $Where must be a non-empty string."
    }
    if ($Path.StartsWith('/') -or $Path.StartsWith('-') -or $Path -cmatch '[\\*?\[\]]') {
        Fail "scripts/sidecars.lock: $Where must be a plain relative path with / separators: '$Path'."
    }
    if ($Path -cmatch '[\x00-\x1f]') {
        Fail "scripts/sidecars.lock: $Where has a control character."
    }
    foreach ($part in $Path.Split('/')) {
        if ($part -eq '' -or $part -eq '.' -or $part -eq '..') {
            Fail "scripts/sidecars.lock: $Where has an empty, '.' or '..' part: '$Path'."
        }
    }
}

function Test-Sha256($Value, [string]$Where) {
    if ($Value -isnot [string] -or $Value -cnotmatch '^[0-9a-f]{64}$') {
        Fail "scripts/sidecars.lock: $Where must be 64 lower-case hex digits."
    }
}

# How an archive is extracted, from the end of its URL.
function Get-ArchiveType([string]$Url, [string]$Where) {
    if ($Url.EndsWith('.zip', [System.StringComparison]::Ordinal)) { return 'zip' }
    if ($Url.EndsWith('.tgz', [System.StringComparison]::Ordinal) -or
        $Url.EndsWith('.tar.gz', [System.StringComparison]::Ordinal)) { return 'tgz' }
    Fail "scripts/sidecars.lock: $Where must end in .zip, .tgz or .tar.gz, so the script knows how to extract it."
}

# A library's name in binaries/: its file name with the target before the
# extension, so bin/pdfium.dll becomes pdfium-x86_64-pc-windows-msvc.dll.
function Get-LibraryFileName([string]$Path, [string]$Target) {
    $file = $Path.Split('/')[-1]
    $dot = $file.LastIndexOf('.')
    if ($dot -gt 0) {
        return $file.Substring(0, $dot) + '-' + $Target + $file.Substring($dot)
    }
    return "$file-$Target"
}

# Reads and checks the whole lock, and returns what Windows needs from it:
# the archives to download and the files to take from them. With a name, only
# that sidecar's or library's.
function Read-Lock([string]$LockPath, $OnlyName) {
    try {
        $lock = [System.IO.File]::ReadAllText($LockPath) | ConvertFrom-Json
    } catch {
        Fail "scripts/sidecars.lock: can't read it as JSON ($($_.Exception.Message))."
    }
    Test-Keys $lock 'the file' @('format', 'archives', 'sidecars', 'libraries')
    $format = Get-Key $lock 'format'
    if (-not ($format -is [int] -or $format -is [long]) -or $format -ne 2) {
        Fail "scripts/sidecars.lock: 'format' must be 2. This script reads format 2 only."
    }

    $nameRe = '^[a-z0-9][a-z0-9-]*$'
    $tripleRe = '^[a-z0-9_]+(-[a-z0-9_]+){2,3}$'
    $archives = @{}
    foreach ($p in (Get-Entries (Get-Key $lock 'archives') 'archives')) {
        $id = $p.Name
        $a = $p.Value
        $where = "archives.$id"
        if ($id -cnotmatch $nameRe) {
            Fail "scripts/sidecars.lock: archive ids are lower-case letters, digits and dashes: '$id'."
        }
        Test-Keys $a $where @('url', 'sha256', 'size')
        $url = Get-Key $a 'url'
        $sha = Get-Key $a 'sha256'
        $size = Get-Key $a 'size'
        if ($url -isnot [string] -or $url -cnotmatch '^https://\S+$') {
            Fail "scripts/sidecars.lock: $where.url must be an https:// URL."
        }
        $type = Get-ArchiveType $url "$where.url"
        Test-Sha256 $sha "$where.sha256"
        if (-not ($size -is [int] -or $size -is [long]) -or $size -le 0) {
            Fail "scripts/sidecars.lock: $where.size must be the archive's size in bytes."
        }
        $archives[$id] = [pscustomobject]@{ Id = $id; Url = $url; Sha256 = $sha; Size = [long]$size; Type = $type }
    }

    $files = @()
    $names = @()
    foreach ($p in (Get-Entries (Get-Key $lock 'sidecars') 'sidecars')) {
        $name = $p.Name
        $s = $p.Value
        $where = "sidecars.$name"
        if ($name -cnotmatch $nameRe) {
            Fail "scripts/sidecars.lock: sidecar names are lower-case letters, digits and dashes: '$name'."
        }
        $names += $name
        Test-Keys $s $where @('version', 'targets')
        $version = Get-Key $s 'version'
        if ($version -isnot [string] -or $version -cnotmatch '^[0-9]+(\.[0-9]+)*$') {
            Fail "scripts/sidecars.lock: $where.version must look like 9.0.2."
        }
        foreach ($tp in (Get-Entries (Get-Key $s 'targets') "$where.targets")) {
            $t = $tp.Value
            $twhere = "$where.targets.$($tp.Name)"
            if ($tp.Name -cnotmatch $tripleRe -or $tp.Name.StartsWith('universal-')) {
                Fail "scripts/sidecars.lock: ${where}: '$($tp.Name)' isn't a target triple Grape fetches."
            }
            Test-Keys $t $twhere @('archive', 'path') @('license')
            $archive = Get-Key $t 'archive'
            if ($archive -isnot [string] -or -not $archives.ContainsKey($archive)) {
                Fail "scripts/sidecars.lock: $twhere.archive names '$archive', which isn't under archives."
            }
            Test-ArchivePath (Get-Key $t 'path') "$twhere.path"
            $license = Get-Key $t 'license'
            if ($null -ne $license) {
                Test-ArchivePath $license "$twhere.license"
            }
            if ($tp.Name -cne $triple) { continue }
            $files += [pscustomobject]@{
                Kind    = 'sidecar'
                Name    = $name
                Version = $version
                Archive = $archive
                Path    = (Get-Key $t 'path')
                License = $license
                Sha256  = $null
                Output  = "$name-$triple.exe"
            }
        }
    }

    foreach ($p in (Get-Entries (Get-Key $lock 'libraries') 'libraries')) {
        $name = $p.Name
        $l = $p.Value
        $where = "libraries.$name"
        if ($name -cnotmatch $nameRe) {
            Fail "scripts/sidecars.lock: library names are lower-case letters, digits and dashes: '$name'."
        }
        if ($names -ccontains $name) {
            Fail "scripts/sidecars.lock: '$name' is both a sidecar and a library."
        }
        $names += $name
        Test-Keys $l $where @('version', 'targets')
        $version = Get-Key $l 'version'
        if ($version -isnot [string] -or $version -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._/-]*$') {
            Fail "scripts/sidecars.lock: $where.version must look like chromium/8086."
        }
        foreach ($tp in (Get-Entries (Get-Key $l 'targets') "$where.targets")) {
            $t = $tp.Value
            $tname = $tp.Name
            $twhere = "$where.targets.$tname"
            $universal = ($tname -ceq 'universal-apple-darwin')
            if ($tname -cnotmatch $tripleRe -or ($tname.StartsWith('universal-') -and -not $universal)) {
                Fail "scripts/sidecars.lock: ${where}: '$tname' isn't a target Grape fetches."
            }
            if ($tname.EndsWith('-apple-darwin') -and -not $universal) {
                Fail "scripts/sidecars.lock: ${where}: give the Mac library as universal-apple-darwin, not '$tname'. The scripts don't merge libraries with lipo."
            }
            Test-Keys $t $twhere @('archive', 'path', 'sha256')
            $archive = Get-Key $t 'archive'
            if ($archive -isnot [string] -or -not $archives.ContainsKey($archive)) {
                Fail "scripts/sidecars.lock: $twhere.archive names '$archive', which isn't under archives."
            }
            $path = Get-Key $t 'path'
            Test-ArchivePath $path "$twhere.path"
            $sha = Get-Key $t 'sha256'
            Test-Sha256 $sha "$twhere.sha256"
            if ($tname -cne $triple) { continue }
            $files += [pscustomobject]@{
                Kind    = 'library'
                Name    = $name
                Version = $version
                Archive = $archive
                Path    = $path
                License = $null
                Sha256  = $sha
                Output  = (Get-LibraryFileName $path $triple)
            }
        }
    }

    if ($null -ne $OnlyName) {
        if ($names -cnotcontains $OnlyName) {
            Fail "scripts/sidecars.lock has no sidecar or library named '$OnlyName'."
        }
        $files = @($files | Where-Object { $_.Name -ceq $OnlyName })
        if ($files.Count -eq 0) {
            Fail "scripts/sidecars.lock: '$OnlyName' has no $triple target."
        }
    }
    if ($files.Count -eq 0) {
        Fail "scripts/sidecars.lock: no sidecar or library has a $triple target."
    }
    $used = @()
    foreach ($f in $files) {
        if ($used -cnotcontains $f.Archive) { $used += $f.Archive }
    }
    return [pscustomobject]@{
        Archives = @($used | ForEach-Object { $archives[$_] })
        Files    = $files
    }
}

# The files this run leaves in binaries/.
function Get-ExpectedFiles($Plan) {
    $names = @()
    foreach ($f in $Plan.Files) {
        $names += $f.Output
        if ($null -ne $f.License) { $names += "$($f.Name)-LICENSE.txt" }
    }
    return @($names | Sort-Object -Unique)
}

# The SHA-256 the stamp lists for each file, when it was written from this
# lock for this target. Otherwise an empty table.
function Read-Stamp([string]$Stamp, [string]$LockHash) {
    $hashes = New-Object System.Collections.Hashtable ([System.StringComparer]::Ordinal)
    if (-not (Test-Path -LiteralPath $Stamp -PathType Leaf)) { return $hashes }
    $lines = @([System.IO.File]::ReadAllLines($Stamp))
    if ($lines -cnotcontains "lock $LockHash") { return $hashes }
    if ($lines -cnotcontains "target $triple") { return $hashes }
    foreach ($line in $lines) {
        if ($line -cmatch '^([0-9a-f]{64})  (.+)$') { $hashes[$Matches[2]] = $Matches[1] }
    }
    return $hashes
}

# True when every file this run should leave is there with the SHA-256 the
# stamp recorded for it.
function Test-StampCurrent($Recorded, [string]$BinDir, [string[]]$Expected) {
    foreach ($file in $Expected) {
        $path = Join-Path $BinDir $file
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return $false }
        if (-not $Recorded.ContainsKey($file)) { return $false }
        if ((Get-Sha256 $path) -cne $Recorded[$file]) { return $false }
    }
    return $true
}

function Save-Archive($Archive, [string]$Dest) {
    $request = @{ Uri = $Archive.Url; OutFile = $Dest; UseBasicParsing = $true }
    if ($PSVersionTable.PSVersion.Major -ge 7) {
        # PowerShell 7 ignores ServicePointManager; it takes the versions here.
        $request['SslProtocol'] = 'Tls12, Tls13'
    }
    $attempt = 0
    while ($true) {
        $attempt++
        try {
            Invoke-WebRequest @request
            return
        } catch {
            Remove-Item -LiteralPath $Dest -Force -ErrorAction SilentlyContinue
            if ($attempt -ge 3) {
                Fail "couldn't download $($Archive.Id) from $($Archive.Url) ($($_.Exception.Message))."
            }
            Write-Host "  The download failed ($($_.Exception.Message)). Trying again."
            Start-Sleep -Seconds 5
        }
    }
}

# Windows' own tar, by its full path. Another OS has one only for testing
# (GRAPE_SIDECARS_NO_RUN=1).
function Get-Tar {
    if ($env:OS -eq 'Windows_NT') {
        if ([string]::IsNullOrEmpty($env:SystemRoot)) {
            Fail "SystemRoot isn't set, so Windows' tar.exe can't be found."
        }
        $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
        $hint = ' Windows 10 1803 and later have it.'
    } else {
        $tar = '/usr/bin/tar'
        $hint = ''
    }
    if (-not (Test-Path -LiteralPath $tar -PathType Leaf)) {
        Fail "$tar is missing.$hint"
    }
    return $tar
}

# Extracts one file the lock names from an archive that was already checked.
function Expand-OneFile($Archive, [string]$File, [string]$Path, [string]$Dest) {
    if ($Archive.Type -ceq 'zip') {
        $zip = [System.IO.Compression.ZipFile]::OpenRead($File)
        try {
            $entry = $zip.GetEntry($Path)
            if ($null -eq $entry -or $entry.FullName.EndsWith('/')) {
                Fail "$($Archive.Id) has no file '$Path' in it."
            }
            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $Dest, $false)
        } finally {
            $zip.Dispose()
        }
        return
    }

    # A .tgz: tar extracts just this path, into a folder for this archive.
    $dir = Join-Path (Join-Path $script:tmp 'x') $Archive.Id
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $tar = Get-Tar
    $previous = $ErrorActionPreference
    # Native programs' stderr must not turn into errors here.
    $ErrorActionPreference = 'Continue'
    try {
        $said = @(& $tar -xzf $File -C $dir $Path 2>&1)
        $exitCode = $LASTEXITCODE
    } catch {
        Fail "tar couldn't start ($($_.Exception.GetBaseException().Message))."
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($exitCode -ne 0) {
        foreach ($line in $said) { [Console]::Error.WriteLine([string]$line) }
        Fail "$($Archive.Id) has no file '$Path' in it, or tar couldn't read it (exit code $exitCode)."
    }
    $src = Join-Path $dir $Path
    $item = Get-Item -LiteralPath $src -Force -ErrorAction SilentlyContinue
    if ($null -eq $item -or $item.PSIsContainer -or
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
        Fail "'$Path' in $($Archive.Id) isn't a plain file."
    }
    Copy-Item -LiteralPath $src -Destination $Dest
}

# Runs a sidecar with -hide_banner -version and checks what it says.
function Test-Version([string]$Exe, [string]$Name, [string]$Version) {
    $previous = $ErrorActionPreference
    # Native programs' stderr must not turn into errors here.
    $ErrorActionPreference = 'Continue'
    try {
        $lines = @(& $Exe -hide_banner -version)
        $exitCode = $LASTEXITCODE
    } catch {
        Fail "$Name couldn't start on this PC ($($_.Exception.GetBaseException().Message))."
    } finally {
        $ErrorActionPreference = $previous
    }
    if ($exitCode -ne 0) {
        Fail "$Name didn't run with -version (exit code $exitCode)."
    }
    if ($lines.Count -eq 0) {
        Fail "$Name printed nothing for -version."
    }
    $first = [string]$lines[0]
    if (-not $first.StartsWith("$Name version ", [System.StringComparison]::Ordinal)) {
        Fail "$Name's first line should start with '$Name version', but it says: $first"
    }
    # The version must be a whole token: 9.0.2 is in
    # "n9.0.2-17-g2a571b6068", but not in "19.0.2" or "9.0.21".
    $tokens = ' ' + ($first -creplace '[^0-9.]', ' ') + ' '
    if (-not $tokens.Contains(" $Version ")) {
        Fail "$Name says '$first', not version $Version."
    }
    $config = @($lines | Where-Object { ([string]$_).StartsWith('configuration:', [System.StringComparison]::Ordinal) })
    if ($config.Count -eq 0) {
        Fail "$Name printed no configuration line."
    }
    if (($config -join ' ').Contains('--enable-nonfree')) {
        Fail "$Name was built with --enable-nonfree, so it can't be shipped."
    }
    Write-Host "  $first"
}

function Invoke-Fetch {
    $noRun = ($env:GRAPE_SIDECARS_NO_RUN -eq '1')
    if ($env:OS -ne 'Windows_NT' -and -not $noRun) {
        Fail 'this script is for Windows. On macOS, run ./scripts/fetch-sidecars.sh.'
    }
    $onlyName = $null
    if ($onlyGiven) {
        if ([string]::IsNullOrEmpty($Only)) {
            Fail '-Only needs the name of a sidecar or library in scripts/sidecars.lock, such as pdfium.'
        }
        $onlyName = $Only
    }

    $repo = Split-Path -Parent $PSScriptRoot
    $lockPath = Join-Path $PSScriptRoot 'sidecars.lock'
    $binDir = Join-Path $repo 'apps/desktop/src-tauri/binaries'
    $stamp = Join-Path $binDir 'sidecars.stamp'
    if (-not (Test-Path -LiteralPath $lockPath -PathType Leaf)) {
        Fail "can't find scripts/sidecars.lock."
    }

    $plan = Read-Lock $lockPath $onlyName
    $expected = Get-ExpectedFiles $plan
    $lockHash = Get-Sha256 $lockPath
    $recorded = Read-Stamp $stamp $lockHash

    if (-not $Force -and (Test-StampCurrent $recorded $binDir $expected)) {
        if ($null -ne $onlyName) {
            Write-Host "$onlyName is already current in apps/desktop/src-tauri/binaries (it matches scripts/sidecars.lock). Pass -Force to fetch it again."
        } else {
            Write-Host 'Sidecars are already current in apps/desktop/src-tauri/binaries (they match scripts/sidecars.lock). Pass -Force to fetch them again.'
        }
        return
    }

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem

    $script:tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("grape-sidecars-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $script:tmp | Out-Null
    $dlDir = Join-Path $script:tmp 'dl'
    $out = Join-Path $script:tmp 'out'
    New-Item -ItemType Directory -Path $dlDir, $out | Out-Null

    # Download each archive and check it before anything reads it.
    $downloaded = @{}
    foreach ($a in $plan.Archives) {
        $file = Join-Path $dlDir "$($a.Id).$($a.Type)"
        Write-Host "Downloading $($a.Id) ($($a.Size) bytes) from $($a.Url)"
        Save-Archive $a $file
        $size = (Get-Item -LiteralPath $file).Length
        if ($size -ne $a.Size) {
            Remove-Item -LiteralPath $file -Force
            Fail "$($a.Id) is $size bytes, but the lock says $($a.Size). Deleted it; nothing was extracted or changed."
        }
        $sha = Get-Sha256 $file
        if ($sha -cne $a.Sha256) {
            Remove-Item -LiteralPath $file -Force
            Fail "$($a.Id) has SHA-256 $sha, but the lock says $($a.Sha256). Deleted it; nothing was extracted or changed."
        }
        Write-Host '  SHA-256 matches the lock.'
        $downloaded[$a.Id] = [pscustomobject]@{ Archive = $a; File = $file }
    }

    # Extract only the paths the lock names. A library's file must have the
    # SHA-256 the lock gives it.
    foreach ($f in $plan.Files) {
        $d = $downloaded[$f.Archive]
        $dest = Join-Path $out $f.Output
        Expand-OneFile $d.Archive $d.File $f.Path $dest
        Write-Host "Extracted $($f.Name) for $triple."
        if ($null -ne $f.License) {
            Expand-OneFile $d.Archive $d.File $f.License (Join-Path $out "$($f.Name)-LICENSE.txt")
        }
        if ($f.Kind -ceq 'library') {
            $sha = Get-Sha256 $dest
            if ($sha -cne $f.Sha256) {
                Fail "'$($f.Path)' in $($f.Archive) has SHA-256 $sha, but the lock says $($f.Sha256). Nothing was changed."
            }
            Write-Host "  $($f.Name) $($f.Version): its SHA-256 matches the lock."
        }
    }

    if ($noRun) {
        Write-Host 'GRAPE_SIDECARS_NO_RUN=1: skipped the -version checks. That''s for testing only.'
    } else {
        foreach ($f in $plan.Files) {
            if ($f.Kind -cne 'sidecar') { continue }
            Test-Version (Join-Path $out $f.Output) $f.Name $f.Version
        }
    }

    # Everything checked: replace the files in binaries/, then write the stamp.
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null
    Remove-Item -LiteralPath $stamp -Force -ErrorAction SilentlyContinue
    foreach ($file in (Get-ChildItem -LiteralPath $out -File)) {
        Move-Item -LiteralPath $file.FullName -Destination (Join-Path $binDir $file.Name) -Force
    }

    if ($noRun) {
        Write-Host 'Placed the files in apps/desktop/src-tauri/binaries and wrote no stamp, so the next run without GRAPE_SIDECARS_NO_RUN fetches and checks everything again.'
        return
    }

    # What this run didn't fetch keeps its line while its file is there. The
    # next run checks that file's SHA-256 against it as usual.
    $entries = New-Object System.Collections.Hashtable ([System.StringComparer]::Ordinal)
    foreach ($file in @($recorded.Keys)) {
        if ($expected -ccontains $file) { continue }
        if (Test-Path -LiteralPath (Join-Path $binDir $file) -PathType Leaf) {
            $entries[$file] = $recorded[$file]
        }
    }
    foreach ($file in $expected) {
        $path = Join-Path $binDir $file
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Fail "$file is missing from binaries/ after the move."
        }
        $entries[$file] = Get-Sha256 $path
    }
    $text = "# Written by scripts/fetch-sidecars.ps1. Delete this file or pass -Force to fetch again.`n"
    $text += "lock $lockHash`n"
    $text += "target $triple`n"
    foreach ($file in (@($entries.Keys) | Sort-Object)) {
        $text += "$($entries[$file])  $file`n"
    }
    # LF and no BOM, the same as the macOS script's stamp.
    [System.IO.File]::WriteAllText($stamp, $text, (New-Object System.Text.UTF8Encoding $false))

    Write-Host 'Sidecars are ready in apps/desktop/src-tauri/binaries:'
    foreach ($file in $expected) {
        $length = (Get-Item -LiteralPath (Join-Path $binDir $file)).Length
        Write-Host ('  {0,-40} {1,12} bytes' -f $file, $length)
    }
}

$script:tmp = $null
$tls = [System.Net.ServicePointManager]::SecurityProtocol
$exitCode = 0
try {
    # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
    [System.Net.ServicePointManager]::SecurityProtocol = $tls -bor [System.Net.SecurityProtocolType]::Tls12
    Invoke-Fetch
} catch {
    [Console]::Error.WriteLine("${me}: $($_.Exception.Message)")
    $exitCode = 1
} finally {
    [System.Net.ServicePointManager]::SecurityProtocol = $tls
    if ($null -ne $script:tmp -and (Test-Path -LiteralPath $script:tmp)) {
        Remove-Item -LiteralPath $script:tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}
exit $exitCode
