# SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
# SPDX-License-Identifier: Apache-2.0

<#
.SYNOPSIS
Benchmark sccache against a CMake + Ninja MSVC build.

.DESCRIPTION
Runs three scenarios and reports wall clock plus sccache's own counters for each:

  cold    empty cache, empty build tree      -> every TU is a miss
  cached  empty build tree, warm cache       -> every TU should be a hit
  touch   warm cache, one hot header edited  -> mixed

Emits JSON so runs can be diffed across commits. Nothing here interprets the
numbers; it records them along with enough environment state to know whether two
runs are comparable at all.

Environment checks run before any timing and abort by default, because the
common failure is not a crash but a silently invalid measurement: SCCACHE_DIR
left on C: while the build tree is on a Dev Drive, or /Z7 not actually reaching
the compiler.

.PARAMETER Sccache
Path to the sccache binary under test. Defaults to target\release\sccache.exe,
falling back to target\debug\sccache.exe.

.PARAMETER Project
Point at an existing CMake project instead of generating one. The tree is not
modified except for the build directory; the touch scenario is skipped unless
-TouchFile is also given.

.PARAMETER TouchFile
File to modify for the touch scenario when using -Project.

.PARAMETER CacheDir
Cache location. Defaults to SCCACHE_DIR, else D:\.cache\sccache-bench.

.PARAMETER Jobs
Ninja parallelism. Defaults to the processor count.

.PARAMETER ZstdLevel
Sets SCCACHE_CACHE_ZSTD_LEVEL. Use to A/B compression against hit latency.

.PARAMETER OutFile
Where to write the JSON result. Defaults to a timestamped file under results\.

.PARAMETER SkipChecks
Record environment findings but run anyway. The findings still appear in the
output, flagged.

.EXAMPLE
.\Invoke-SccacheBench.ps1
.EXAMPLE
.\Invoke-SccacheBench.ps1 -ZstdLevel 1 -OutFile results\zstd1.json
#>
[CmdletBinding()]
param(
    [string] $Sccache,
    [string] $Project,
    [string] $TouchFile,
    [string] $CacheDir,
    [int]    $Jobs = 0,
    [int]    $ZstdLevel = 0,
    [string] $OutFile,
    [int]    $TranslationUnits = 40,
    [int]    $Headers = 20,
    [int]    $IncludesPerUnit = 8,
    [int]    $Weight = 60,
    [switch] $SkipChecks
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if ($Jobs -le 0) { $Jobs = [Environment]::ProcessorCount }

# --- locate sccache -------------------------------------------------------

if (-not $Sccache) {
    foreach ($candidate in @('target\release\sccache.exe', 'target\debug\sccache.exe')) {
        $p = Join-Path $repoRoot $candidate
        if (Test-Path $p) { $Sccache = $p; break }
    }
}
if (-not $Sccache -or -not (Test-Path $Sccache)) {
    throw "sccache binary not found. Build it (cargo build --release) or pass -Sccache."
}
$Sccache = (Resolve-Path $Sccache).Path

# --- MSVC developer environment ------------------------------------------

function Import-VsDevEnv {
    if ($env:VSCMD_ARG_TGT_ARCH) { return $true }   # already inside a dev prompt
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path $vswhere)) { return $false }
    $install = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath 2>$null
    if (-not $install) { return $false }
    $vcvars = Join-Path $install 'VC\Auxiliary\Build\vcvarsall.bat'
    if (-not (Test-Path $vcvars)) { return $false }

    # vcvarsall only exports into its own cmd process, so run it and re-import
    # the resulting environment.
    cmd /c "`"$vcvars`" x64 >nul 2>&1 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
        }
    }
    return [bool]$env:VSCMD_ARG_TGT_ARCH
}

if (-not (Import-VsDevEnv)) {
    throw "Could not locate an MSVC toolchain via vswhere. Run from a Developer PowerShell, or install the C++ workload."
}

foreach ($tool in 'cmake', 'ninja', 'cl') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Required tool '$tool' is not on PATH."
    }
}

# --- cache + project layout ----------------------------------------------

if (-not $CacheDir) {
    $CacheDir = if ($env:SCCACHE_DIR) { $env:SCCACHE_DIR } else { 'D:\.cache\sccache-bench' }
}
$env:SCCACHE_DIR = $CacheDir

$generated = $null
if ($Project) {
    $projectDir = (Resolve-Path $Project).Path
} else {
    $projectDir = Join-Path $repoRoot 'target\bench-project'
    New-Item -ItemType Directory -Force $projectDir | Out-Null
    $generated = & (Join-Path $PSScriptRoot 'New-SyntheticProject.ps1') `
        -Path $projectDir -TranslationUnits $TranslationUnits -Headers $Headers `
        -IncludesPerUnit $IncludesPerUnit -Weight $Weight
    $TouchFile = Join-Path $projectDir 'include\bench_hot.h'
}
$buildDir = Join-Path $projectDir 'build'

# --- environment findings -------------------------------------------------

function Get-VolumeInfo {
    param([string] $ForPath)
    $root = [System.IO.Path]::GetPathRoot((Resolve-Path -LiteralPath $ForPath -ErrorAction SilentlyContinue))
    if (-not $root) { $root = [System.IO.Path]::GetPathRoot($ForPath) }
    $letter = $root.TrimEnd('\', ':')
    $vol = Get-Volume -DriveLetter $letter -ErrorAction SilentlyContinue
    $devdrv = (fsutil devdrv query "${letter}:" 2>&1) -join ' '
    [pscustomobject]@{
        Root       = $root
        FileSystem = if ($vol) { $vol.FileSystemType } else { 'unknown' }
        DevDrive   = if ($devdrv -match 'is a trusted developer volume') { 'trusted' }
                     elseif ($devdrv -match 'not a developer volume') { 'no' }
                     elseif ($devdrv -match 'Access is denied') { 'unknown (needs elevation)' }
                     else { $devdrv.Trim() }
    }
}

New-Item -ItemType Directory -Force $CacheDir | Out-Null
$cacheVol = Get-VolumeInfo -ForPath $CacheDir
$buildVol = Get-VolumeInfo -ForPath $projectDir

$findings = @()
function Add-Finding {
    param([string] $Level, [string] $Message)
    $script:findings += [pscustomobject]@{ level = $Level; message = $Message }
}

if ($cacheVol.Root -ne $buildVol.Root) {
    Add-Finding 'error' ("Cache ($($cacheVol.Root)) and build tree ($($buildVol.Root)) are on different volumes. " +
        'Block cloning requires one volume, and cross-volume numbers are not comparable to same-volume ones.')
}
if ($cacheVol.FileSystem -ne 'ReFS') {
    Add-Finding 'warning' "Cache volume is $($cacheVol.FileSystem), not ReFS. Block cloning is unavailable here."
}
if ($cacheVol.DevDrive -notin @('trusted')) {
    Add-Finding 'info' "Cache volume Dev Drive status: $($cacheVol.DevDrive). Defender async scanning may not apply."
}

# --- helpers --------------------------------------------------------------

function Invoke-Sccache {
    param([Parameter(ValueFromRemainingArguments)] $Args)
    & $Sccache @Args 2>&1
}

function Get-Stats {
    $raw = & $Sccache --show-stats --stats-format=json 2>&1 | Out-String
    try { return $raw | ConvertFrom-Json } catch { throw "Could not parse sccache stats: $raw" }
}

function Restart-Server {
    param([switch] $WipeCache)
    & $Sccache --stop-server 2>&1 | Out-Null
    if ($WipeCache -and (Test-Path $CacheDir)) {
        Remove-Item -Recurse -Force $CacheDir -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force $CacheDir | Out-Null
    }
    & $Sccache --start-server 2>&1 | Out-Null
}

function Invoke-Configure {
    if (Test-Path $buildDir) { Remove-Item -Recurse -Force $buildDir }
    New-Item -ItemType Directory -Force $buildDir | Out-Null
    $cmakeArgs = @(
        '-S', $projectDir, '-B', $buildDir, '-G', 'Ninja',
        '-DCMAKE_BUILD_TYPE=Debug',
        '-DCMAKE_C_COMPILER=cl', '-DCMAKE_CXX_COMPILER=cl',
        "-DCMAKE_C_COMPILER_LAUNCHER=$Sccache",
        "-DCMAKE_CXX_COMPILER_LAUNCHER=$Sccache",
        '-DCMAKE_MSVC_DEBUG_INFORMATION_FORMAT=Embedded',
        '-DCMAKE_EXPORT_COMPILE_COMMANDS=ON'
    )
    $out = & cmake @cmakeArgs 2>&1
    if ($LASTEXITCODE -ne 0) { throw "cmake configure failed:`n$($out -join "`n")" }
}

function Test-EmbeddedDebugInfo {
    $cc = Join-Path $buildDir 'compile_commands.json'
    if (-not (Test-Path $cc)) { return $null }
    $text = Get-Content $cc -Raw
    # /Z7 is what makes MSVC output cacheable; /Zi writes a shared PDB and sccache
    # refuses to cache it ("shared pdb").
    return ($text -match '(?<![\w/])[/-]Z7(?![\w])')
}

function Invoke-Build {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $out = & ninja -C $buildDir -j $Jobs 2>&1
    $sw.Stop()
    if ($LASTEXITCODE -ne 0) { throw "ninja build failed:`n$($out -join "`n")" }
    return $sw.Elapsed.TotalSeconds
}

function New-ScenarioResult {
    param([string] $Name, [double] $Seconds, $Stats, [string[]] $Notes = @())
    $s = $Stats.stats
    [pscustomobject]@{
        name                      = $Name
        wall_seconds              = [math]::Round($Seconds, 3)
        compile_requests          = $s.compile_requests
        cache_hits                = [int](($s.cache_hits.counts.PSObject.Properties | Measure-Object -Property Value -Sum).Sum)
        cache_misses              = [int](($s.cache_misses.counts.PSObject.Properties | Measure-Object -Property Value -Sum).Sum)
        preprocessor_cache_hits   = $s.preprocessor_cache_hits
        preprocessor_cache_misses = $s.preprocessor_cache_misses
        cache_write_errors        = $s.cache_write_errors
        cache_read_errors         = $s.cache_read_errors
        compilations              = $s.compilations
        forced_recaches           = $s.forced_recaches
        notes                     = $Notes
    }
}

# --- scenarios ------------------------------------------------------------

if ($ZstdLevel -gt 0) { $env:SCCACHE_CACHE_ZSTD_LEVEL = "$ZstdLevel" }

Write-Host "sccache : $Sccache"
Write-Host "project : $projectDir"
Write-Host "cache   : $CacheDir ($($cacheVol.FileSystem), dev drive: $($cacheVol.DevDrive))"
Write-Host "jobs    : $Jobs"
Write-Host ""

foreach ($f in $findings) {
    $color = if ($f.level -eq 'error') { 'Red' } elseif ($f.level -eq 'warning') { 'Yellow' } else { 'DarkGray' }
    Write-Host "[$($f.level)] $($f.message)" -ForegroundColor $color
}
if (($findings | Where-Object level -eq 'error') -and -not $SkipChecks) {
    throw "Environment checks failed. Fix the above, or pass -SkipChecks to record and continue."
}

$scenarios = @()

# cold: nothing cached, nothing built
Write-Host "`n[cold] empty cache, empty build tree" -ForegroundColor Cyan
Restart-Server -WipeCache
Invoke-Configure
$z7 = Test-EmbeddedDebugInfo
if ($z7 -eq $false) {
    Add-Finding 'error' 'No /Z7 in compile_commands.json. MSVC output will not be cacheable (shared PDB).'
    if (-not $SkipChecks) { throw 'Configured build is not using embedded debug info; aborting.' }
}
& $Sccache --zero-stats 2>&1 | Out-Null
$t = Invoke-Build
$scenarios += New-ScenarioResult 'cold' $t (Get-Stats)
Write-Host ("  {0:N2}s" -f $t)

# cached: same sources, warm cache, fresh build tree
Write-Host "`n[cached] warm cache, empty build tree" -ForegroundColor Cyan
Invoke-Configure
& $Sccache --zero-stats 2>&1 | Out-Null
$t = Invoke-Build
$cachedStats = Get-Stats
$notes = @()

# The regression test for the MSVC direct-mode bug: the mode reports as enabled
# but, before the -E and #line fixes, could never hit. A fully cached rebuild
# with zero preprocessor cache hits means every TU still ran the preprocessor.
if ($cachedStats.use_preprocessor_cache_mode -and
    $cachedStats.stats.preprocessor_cache_hits -eq 0 -and
    $cachedStats.stats.preprocessor_cache_misses -gt 0) {
    $msg = 'Direct mode reports enabled but recorded 0 hits on a fully cached rebuild: ' +
           'the preprocessor ran for every translation unit.'
    Add-Finding 'error' $msg
    $notes += $msg
}
$scenarios += New-ScenarioResult 'cached' $t $cachedStats $notes
Write-Host ("  {0:N2}s" -f $t)

# touch: invalidate one widely-included header
if ($TouchFile -and (Test-Path $TouchFile)) {
    Write-Host "`n[touch] warm cache, one header edited" -ForegroundColor Cyan
    $original = [System.IO.File]::ReadAllText($TouchFile)
    try {
        [System.IO.File]::WriteAllText($TouchFile, $original + "`n// bench touch $(Get-Random)`n")
        Invoke-Configure
        & $Sccache --zero-stats 2>&1 | Out-Null
        $t = Invoke-Build
        $scenarios += New-ScenarioResult 'touch' $t (Get-Stats)
        Write-Host ("  {0:N2}s" -f $t)
    } finally {
        [System.IO.File]::WriteAllText($TouchFile, $original)
    }
} else {
    Write-Host "`n[touch] skipped (no -TouchFile)" -ForegroundColor DarkGray
}

# --- output ---------------------------------------------------------------

$version = (& $Sccache --version 2>&1 | Out-String).Trim()
$commit = (& git -C $repoRoot rev-parse --short HEAD 2>$null | Out-String).Trim()

$result = [pscustomobject]@{
    timestamp   = (Get-Date).ToString('o')
    sccache     = [pscustomobject]@{ path = $Sccache; version = $version; commit = $commit }
    environment = [pscustomobject]@{
        cache_dir        = $CacheDir
        cache_volume     = $cacheVol
        project_dir      = $projectDir
        project_volume   = $buildVol
        jobs             = $Jobs
        zstd_level       = if ($ZstdLevel -gt 0) { $ZstdLevel } else { $null }
        embedded_debug   = $z7
        generated        = $generated
        cl               = (Get-Command cl).Source
    }
    findings    = $findings
    scenarios   = $scenarios
}

if (-not $OutFile) {
    $resultsDir = Join-Path $PSScriptRoot 'results'
    New-Item -ItemType Directory -Force $resultsDir | Out-Null
    $OutFile = Join-Path $resultsDir ("bench-{0}.json" -f (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
$result | ConvertTo-Json -Depth 8 | Set-Content -Path $OutFile -Encoding UTF8

Write-Host "`n--- summary ---" -ForegroundColor Cyan
$scenarios | Format-Table name, wall_seconds, cache_hits, cache_misses,
    preprocessor_cache_hits, preprocessor_cache_misses -AutoSize | Out-String | Write-Host
Write-Host "written: $OutFile"

if ($findings | Where-Object level -eq 'error') { exit 1 }
