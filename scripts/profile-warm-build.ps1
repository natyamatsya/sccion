# SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
# SPDX-License-Identifier: Apache-2.0

<#
.SYNOPSIS
    Capture an ETW profile of one warm build, across every process it spawns.

.DESCRIPTION
    A build's time is not spent in one process. In a warm build of a large C++ product the
    compile step is under 40% of the CPU, and the rest is AUTOMOC, the linker and
    a long tail of custom commands -- none of which a profiler attached to
    sccache would ever see. ETW samples every process on the machine, so the
    resulting trace can answer "where did the time go" for the build rather than
    for one binary in it.

    MUST BE RUN ELEVATED. ETW CPU sampling is an administrative operation; there
    is no unprivileged equivalent.

    The build itself is run by this script so that the capture brackets exactly
    one build and nothing else.

.PARAMETER BuildDir
    Build directory to build, relative to -SourceDir.

.PARAMETER SourceDir
    Checkout to build in.

.PARAMETER Out
    Where to write the .etl. Expect several GB for a multi-minute build.

.PARAMETER Clean
    Run `--target clean` first, so the capture covers a full warm rebuild rather
    than a no-op.

.PARAMETER FileIO
    Also record file I/O. Off by default: it is very high volume, it roughly
    tripled the build (672s against a 241s warm baseline), and combining it with
    CPU in file mode produced a composite profile that WPR could not stop
    ("CPUFileIO.Verbose.File", 0x80071069 ERROR_WMI_INSTANCE_NOT_FOUND) --
    losing the whole trace after paying for the entire build. CPU sampling alone
    answers both "where did the time go" and the per-process split.

.EXAMPLE
    .\profile-warm-build.ps1 -SourceDir D:\src\product `
        -BuildDir build\test-win-x64-reldbg-trim -Out D:\warm.etl -Clean
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $SourceDir,
    [Parameter(Mandatory)] [string] $BuildDir,
    [string] $Out = "D:\warm-build.etl",
    [switch] $Clean,
    [switch] $FileIO
)

$ErrorActionPreference = 'Stop'

$id = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Must run elevated: ETW CPU sampling requires administrative privileges."
}
if (-not (Get-Command cl.exe -ErrorAction SilentlyContinue)) {
    throw "cl.exe is not on PATH. Run `. .\Init-DevEnv.ps1` in this elevated shell first."
}

# Measured at roughly 139 MB/s on 32 busy cores, so a multi-minute build wants
# tens of GB. Refuse up front rather than lose the build to a full disk at the
# merge, which is where it would otherwise surface.
$outDrive = (Split-Path -Qualifier (Resolve-Path -LiteralPath (Split-Path $Out -Parent) -ErrorAction SilentlyContinue).Path) -replace ':', ''
if ($outDrive) {
    $freeGiB = (Get-PSDrive $outDrive).Free / 1GB
    Write-Host ("Free space on {0}: {1:N1} GiB" -f $outDrive, $freeGiB)
    if ($freeGiB -lt 100) {
        throw "Only $([math]::Round($freeGiB,1)) GiB free on ${outDrive}:; a full build's trace needs well over that."
    }
}

# A stray recording from an earlier run would silently merge into this one.
& wpr -cancel 2>&1 | Out-Null

$profiles = @('-start', 'CPU')
if ($FileIO) { $profiles += @('-start', 'FileIO') }
Write-Host ("Starting ETW capture ({0})..." -f $(if ($FileIO) { 'CPU + file I/O' } else { 'CPU sampling' }))
# -filemode, not the default in-memory ring: a multi-minute build would wrap the
# buffer and leave only the last few seconds, which is the part nobody needs.
#
# Volume is large -- roughly 139 MB/s on 32 busy cores, so tens of GB across a
# build. Check free space before running rather than discovering it at the merge.
& wpr @profiles -filemode
if ($LASTEXITCODE -ne 0) { throw "wpr -start failed ($LASTEXITCODE)" }

# Confirm it is actually recording. `-start` has returned success while leaving
# nothing to stop, and that failure surfaces only after the build has been paid
# for in full.
$status = (& wpr -status 2>&1) -join ' '
if ($status -notmatch 'recording is in progress') {
    & wpr -cancel 2>&1 | Out-Null
    throw "wpr reported success but is not recording; refusing to run the build for a trace that will not exist."
}

try {
    Push-Location $SourceDir
    if ($Clean) {
        Write-Host "Cleaning $BuildDir..."
        & cmake --build $BuildDir --target clean 2>&1 | Out-Null
    }
    if (Get-Command sccache.exe -ErrorAction SilentlyContinue) { & sccache --zero-stats 2>&1 | Out-Null }

    Write-Host "Building..."
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & cmake --build $BuildDir 2>&1 | Tee-Object "$Out.build.log" | Select-Object -Last 3
    $rc = $LASTEXITCODE
    $sw.Stop()
    Write-Host ("Build exit={0} elapsed={1:N1}s" -f $rc, $sw.Elapsed.TotalSeconds)
    if (Get-Command sccache.exe -ErrorAction SilentlyContinue) { & sccache --show-stats | Out-File "$Out.stats.txt" }
}
finally {
    Pop-Location
    Write-Host "Stopping capture and writing $Out (this takes a while)..."
    # The stop performs a merge, which is what injects the image identification
    # records. Without those no PDB can be matched at decode time and every
    # frame comes back as a bare address.
    & wpr -stop $Out
    $stopRc = $LASTEXITCODE
    if ($stopRc -ne 0) {
        Write-Warning "wpr -stop failed ($stopRc)"
        # Leave nothing recording behind, or the next capture inherits it.
        & wpr -cancel 2>&1 | Out-Null
    }
}

if (Test-Path $Out) {
    Write-Host ("Wrote {0} ({1:N2} GiB)" -f $Out, ((Get-Item $Out).Length / 1GB))
    Write-Host "Next: analyse with wpaexporter, or open in wpa.exe"
} else {
    Write-Warning "No trace was written. The build ran and its time is gone; do not re-run without establishing why."
}
