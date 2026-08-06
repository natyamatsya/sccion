// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Generation of a deterministic CMake + Ninja C++ project to measure against.
//!
//! Same parameters always produce byte-identical sources, so runs are comparable
//! across machines and across commits.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectSpec {
    pub translation_units: usize,
    pub headers: usize,
    pub includes_per_unit: usize,
    pub weight: usize,
}

impl Default for ProjectSpec {
    fn default() -> Self {
        Self {
            translation_units: 40,
            headers: 20,
            includes_per_unit: 8,
            weight: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GeneratedProject {
    pub path: PathBuf,
    pub spec: ProjectSpec,
    /// The header every unit includes; touching it invalidates the whole build.
    pub hot_header: PathBuf,
}

/// Write the project. LF endings throughout, so a checkout's line-ending policy
/// can't change what the compiler sees and therefore what gets hashed.
fn write_file(path: &Path, contents: &str) -> Result<()> {
    let normalized = contents.replace("\r\n", "\n");
    fs::write(path, normalized).with_context(|| format!("writing {}", path.display()))
}

pub fn generate(root: &Path, spec: &ProjectSpec) -> Result<GeneratedProject> {
    if spec.includes_per_unit > spec.headers {
        bail!(
            "includes_per_unit ({}) cannot exceed headers ({})",
            spec.includes_per_unit,
            spec.headers
        );
    }

    let src = root.join("src");
    let inc = root.join("include");
    fs::create_dir_all(&src)?;
    fs::create_dir_all(&inc)?;

    for h in 0..spec.headers {
        write_file(
            &inc.join(format!("bench_h{h}.h")),
            &header_source(h, spec.weight),
        )?;
    }

    let hot_header = inc.join("bench_hot.h");
    write_file(
        &hot_header,
        "#pragma once\n\
         // Touched by the 'touch' scenario. Included by every translation unit, so a\n\
         // change here should miss on all of them.\n\
         #define BENCH_HOT_REVISION 0\n\
         namespace bench_hot { inline constexpr int revision = BENCH_HOT_REVISION; }\n",
    )?;

    let mut unit_names = Vec::new();
    for t in 0..spec.translation_units {
        let picked = pick_headers(t, spec);
        write_file(&src.join(format!("unit_{t}.cpp")), &unit_source(t, &picked))?;
        unit_names.push(format!("unit_{t}"));
    }

    write_file(&src.join("main.cpp"), &main_source(&unit_names))?;
    write_file(&root.join("CMakeLists.txt"), &cmake_source(&unit_names))?;
    write_file(&root.join(".gitignore"), "build/\n")?;

    Ok(GeneratedProject {
        path: root.to_path_buf(),
        spec: spec.clone(),
        hot_header,
    })
}

/// Deterministic, well-spread selection: a fixed stride per unit rather than a
/// RNG, so there is no seed to carry around and reproduce.
fn pick_headers(unit: usize, spec: &ProjectSpec) -> Vec<usize> {
    let mut picked: Vec<usize> = (0..spec.includes_per_unit)
        .map(|k| ((unit * 7) + (k * 3)) % spec.headers)
        .collect();
    picked.sort_unstable();
    picked.dedup();
    picked
}

fn header_source(h: usize, weight: usize) -> String {
    format!(
        r#"#pragma once
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

namespace bench_h{h} {{

// Recursive instantiation, so a miss costs real time rather than being lost in
// process-spawn noise. Depth is a generation parameter.
template <std::size_t N> struct fib {{
    static constexpr std::size_t value = fib<N - 1>::value + fib<N - 2>::value;
}};
template <> struct fib<0> {{ static constexpr std::size_t value = 0; }};
template <> struct fib<1> {{ static constexpr std::size_t value = 1; }};

template <std::size_t N> struct chain {{
    static constexpr std::size_t value = N + chain<N - 1>::value;
}};
template <> struct chain<0> {{ static constexpr std::size_t value = 0; }};

inline constexpr std::size_t weight = chain<{weight}>::value + fib<24>::value;

// Instantiate genuinely expensive standard templates, so compile cost is
// representative rather than a microbenchmark of constexpr evaluation.
inline std::size_t consume() {{
    std::array<std::size_t, 16> a{{}};
    for (std::size_t i = 0; i < a.size(); ++i) {{ a[i] = i * weight; }}

    std::vector<std::pair<std::string, std::size_t>> v;
    v.reserve(a.size());
    for (std::size_t i = 0; i < a.size(); ++i) {{
        std::ostringstream os;
        os << "k" << i << "_h{h}";
        v.emplace_back(os.str(), a[i]);
    }}
    std::sort(v.begin(), v.end(), [](auto const& l, auto const& r) {{ return l.second < r.second; }});

    std::map<std::string, std::size_t> m(v.begin(), v.end());
    std::unordered_map<std::string, std::size_t> um(v.begin(), v.end());
    std::function<std::size_t(std::size_t)> f = [](std::size_t x) {{ return x + weight; }};

    std::size_t s = 0;
    for (auto const& kv : m) {{ s += f(kv.second); }}
    for (auto const& kv : um) {{ s += kv.second; }}
    return s;
}}

}} // namespace bench_h{h}
"#
    )
}

fn unit_source(t: usize, picked: &[usize]) -> String {
    let includes = picked
        .iter()
        .map(|h| format!("#include \"bench_h{h}.h\""))
        .collect::<Vec<_>>()
        .join("\n");
    let calls = picked
        .iter()
        .map(|h| format!("    s += bench_h{h}::consume();"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"#include "bench_hot.h"
{includes}

#include <cstddef>

namespace {{
std::size_t unit_{t}_work() {{
    std::size_t s = static_cast<std::size_t>(bench_hot::revision);
{calls}
    return s;
}}
}} // namespace

std::size_t unit_{t}() {{ return unit_{t}_work(); }}
"#
    )
}

fn main_source(units: &[String]) -> String {
    let externs = units
        .iter()
        .map(|u| format!("std::size_t {u}();"))
        .collect::<Vec<_>>()
        .join("\n");
    let sums = units
        .iter()
        .map(|u| format!("    total += {u}();"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"#include <cstddef>
#include <cstdio>

{externs}

int main() {{
    std::size_t total = 0;
{sums}
    std::printf("%zu\n", total);
    return 0;
}}
"#
    )
}

fn cmake_source(units: &[String]) -> String {
    let sources = std::iter::once("main".to_string())
        .chain(units.iter().cloned())
        .map(|u| format!("    src/{u}.cpp"))
        .collect::<Vec<_>>()
        .join("\n");

    // CMP0141 (CMake 3.25) is what makes CMAKE_MSVC_DEBUG_INFORMATION_FORMAT take
    // effect. Without it CMake bakes /Zi into the flags, MSVC writes a PDB shared
    // across translation units, and sccache refuses to cache at all ("shared pdb").
    format!(
        r#"cmake_minimum_required(VERSION 3.25)
project(sccache_bench CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
cmake_policy(SET CMP0141 NEW)

add_executable(sccache_bench
{sources}
)
target_include_directories(sccache_bench PRIVATE include)
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_are_deterministic_and_within_range() {
        let spec = ProjectSpec::default();
        for unit in 0..spec.translation_units {
            let a = pick_headers(unit, &spec);
            let b = pick_headers(unit, &spec);
            assert_eq!(a, b, "selection must be reproducible");
            assert!(a.iter().all(|h| *h < spec.headers));
            assert!(!a.is_empty());
        }
    }

    #[test]
    fn includes_per_unit_cannot_exceed_pool() {
        let dir = std::env::temp_dir().join("sccache-bench-spec-check");
        let spec = ProjectSpec {
            headers: 2,
            includes_per_unit: 4,
            ..ProjectSpec::default()
        };
        assert!(generate(&dir, &spec).is_err());
    }
}
