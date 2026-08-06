# SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
# SPDX-License-Identifier: Apache-2.0

<#
.SYNOPSIS
Generate a deterministic CMake + Ninja C++ project for benchmarking sccache.

.DESCRIPTION
The generated tree is intentionally boring and reproducible: the same parameters
always produce byte-identical sources, so numbers are comparable across runs and
across machines.

Shape is chosen to exercise the paths that matter for a Windows/MSVC cache:

  - A pool of shared headers, each included by many translation units. Header
    fan-out is what makes preprocessing expensive, and preprocessing is what
    direct mode exists to skip.
  - Enough template instantiation per header that a compile costs real time,
    so a cache hit is measurably different from a miss rather than lost in
    process-spawn noise.
  - One "hot" header included by every TU, so the touch scenario can invalidate
    a known fraction of the build.

.PARAMETER Path
Directory to generate into. Created if missing; existing generated files are
overwritten.

.PARAMETER TranslationUnits
Number of .cpp files. Default 40.

.PARAMETER Headers
Size of the shared header pool. Default 20.

.PARAMETER IncludesPerUnit
How many pool headers each TU includes. Default 8.

.PARAMETER Weight
Template instantiation depth per header. Higher costs more compile time per TU.
Default 60.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Path,
    [int] $TranslationUnits = 40,
    [int] $Headers = 20,
    [int] $IncludesPerUnit = 8,
    [int] $Weight = 60
)

$ErrorActionPreference = 'Stop'

if ($IncludesPerUnit -gt $Headers) {
    throw "IncludesPerUnit ($IncludesPerUnit) cannot exceed Headers ($Headers)."
}

$srcDir = Join-Path $Path 'src'
$incDir = Join-Path $Path 'include'
New-Item -ItemType Directory -Force $srcDir | Out-Null
New-Item -ItemType Directory -Force $incDir | Out-Null

# Written with LF endings throughout: CRLF would make the generated tree differ
# between machines depending on git settings, and the whole point is determinism.
function Write-TextFile {
    param([string] $FilePath, [string] $Content)
    [System.IO.File]::WriteAllText($FilePath, ($Content -replace "`r`n", "`n"))
}

# --- shared headers -------------------------------------------------------

for ($h = 0; $h -lt $Headers; $h++) {
    $body = @"
#pragma once
// Standard library headers deliberately, and this is most of the point: in real
// C++ the preprocessor's cost is dominated by pulling in headers like these, not
// by the project's own code. Direct mode exists to skip exactly this work, so a
// benchmark whose headers are cheap cannot measure whether it helps.
#include <algorithm>
#include <array>
#include <cstddef>
#include <functional>
#include <map>
#include <memory>
#include <sstream>
#include <string>
#include <type_traits>
#include <unordered_map>
#include <vector>

namespace bench_h$h {

// Recursive instantiation: the cost here is what makes a miss distinguishable
// from a hit. Depth is a generation parameter so the harness can scale it.
template <std::size_t N> struct fib {
    static constexpr std::size_t value = fib<N - 1>::value + fib<N - 2>::value;
};
template <> struct fib<0> { static constexpr std::size_t value = 0; };
template <> struct fib<1> { static constexpr std::size_t value = 1; };

template <std::size_t N> struct chain {
    using type = std::conditional_t<(N % 2 == 0), chain<N - 1>, chain<N - 1>>;
    static constexpr std::size_t value = N + chain<N - 1>::value;
};
template <> struct chain<0> { using type = void; static constexpr std::size_t value = 0; };

inline constexpr std::size_t weight = chain<$Weight>::value + fib<24>::value;

template <typename T> struct tagged { T v; constexpr explicit tagged(T x) : v(x) {} };

// Instantiate a few genuinely expensive standard templates, so the compile cost
// is representative rather than a microbenchmark of constexpr evaluation.
inline std::size_t consume() {
    std::array<std::size_t, 16> a{};
    for (std::size_t i = 0; i < a.size(); ++i) { a[i] = i * weight; }

    std::vector<std::pair<std::string, std::size_t>> v;
    v.reserve(a.size());
    for (std::size_t i = 0; i < a.size(); ++i) {
        std::ostringstream os;
        os << "k" << i << "_h$h";
        v.emplace_back(os.str(), a[i]);
    }
    std::sort(v.begin(), v.end(), [](auto const& l, auto const& r) { return l.second < r.second; });

    std::map<std::string, std::size_t> m(v.begin(), v.end());
    std::unordered_map<std::string, std::size_t> um(v.begin(), v.end());
    std::function<std::size_t(std::size_t)> f = [](std::size_t x) { return x + weight; };

    std::size_t s = 0;
    for (auto const& kv : m) { s += f(kv.second); }
    for (auto const& kv : um) { s += kv.second; }
    return s;
}

} // namespace bench_h$h
"@
    Write-TextFile (Join-Path $incDir "bench_h$h.h") $body
}

# The hot header: included by every TU, so touching it invalidates everything.
Write-TextFile (Join-Path $incDir 'bench_hot.h') @"
#pragma once
// Touched by the 'touch' scenario. Included by every translation unit, so a
// change here should miss on all of them.
#define BENCH_HOT_REVISION 0
namespace bench_hot { inline constexpr int revision = BENCH_HOT_REVISION; }
"@

# --- translation units ----------------------------------------------------

$unitNames = @()
for ($t = 0; $t -lt $TranslationUnits; $t++) {
    # Deterministic, well-spread header selection: a fixed stride per unit
    # rather than a RNG, so no seed has to be carried around.
    $picked = @()
    for ($k = 0; $k -lt $IncludesPerUnit; $k++) {
        $picked += (($t * 7) + ($k * 3)) % $Headers
    }
    $picked = $picked | Select-Object -Unique

    $includes = ($picked | ForEach-Object { "#include `"bench_h$_.h`"" }) -join "`n"
    $calls = ($picked | ForEach-Object { "    s += bench_h$_::consume();" }) -join "`n"

    $body = @"
#include "bench_hot.h"
$includes

#include <cstddef>

namespace {
std::size_t unit_${t}_work() {
    std::size_t s = static_cast<std::size_t>(bench_hot::revision);
$calls
    return s;
}
} // namespace

std::size_t unit_$t() { return unit_${t}_work(); }
"@
    Write-TextFile (Join-Path $srcDir "unit_$t.cpp") $body
    $unitNames += "unit_$t"
}

# --- main + build files ---------------------------------------------------

$externs = ($unitNames | ForEach-Object { "std::size_t $_();" }) -join "`n"
$sums = ($unitNames | ForEach-Object { "    total += $_();" }) -join "`n"

Write-TextFile (Join-Path $srcDir 'main.cpp') @"
#include <cstddef>
#include <cstdio>

$externs

int main() {
    std::size_t total = 0;
$sums
    std::printf("%zu\n", total);
    return 0;
}
"@

$sourceList = (@('main') + $unitNames | ForEach-Object { "    src/$_.cpp" }) -join "`n"

# CMP0141 (introduced in 3.25) is what makes CMAKE_MSVC_DEBUG_INFORMATION_FORMAT
# take effect; without it CMake bakes /Zi into the flags and MSVC caching breaks
# because the PDB is shared across TUs.
Write-TextFile (Join-Path $Path 'CMakeLists.txt') @"
cmake_minimum_required(VERSION 3.25)
project(sccache_bench CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
cmake_policy(SET CMP0141 NEW)

add_executable(sccache_bench
$sourceList
)
target_include_directories(sccache_bench PRIVATE include)
"@

Write-TextFile (Join-Path $Path '.gitignore') @"
build/
"@

[pscustomobject]@{
    Path             = (Resolve-Path $Path).Path
    TranslationUnits = $TranslationUnits + 1  # + main.cpp
    Headers          = $Headers + 1           # + bench_hot.h
    IncludesPerUnit  = $IncludesPerUnit
    Weight           = $Weight
}
