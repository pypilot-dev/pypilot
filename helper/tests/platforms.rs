//! Wheel-tag resolution on every platform the engine targets, not just the
//! Linux x86-64 the other suites pin.
//!
//! Each fixture is a real PyPI release (see its `_comment` for where and when
//! it was recorded) chosen because its wheel set differs between platforms:
//! Windows-only, macOS-only, no Windows wheels, no macOS x64, no Windows
//! arm64, stable-ABI, free-threaded builds. The platform is always passed in,
//! so these run the same on any CI runner.

use std::path::PathBuf;

use pypilot::core::platform::{Arch, Os, Platform};
use pypilot::core::project::Requirement;
use pypilot::core::torch_gpu::{classify, TorchBuild};
use pypilot::pypi::metadata::{PackageAnalysis, PackageMetadata, SdistKind};
use pypilot::pypi::pyversion::PyVersion;
use pypilot::pypi::{analyze, FixtureSource};

const LINUX_X64: Platform = Platform {
    os: Os::Linux,
    arch: Arch::X86_64,
};
const LINUX_ARM64: Platform = Platform {
    os: Os::Linux,
    arch: Arch::Aarch64,
};
const MAC_X64: Platform = Platform {
    os: Os::Macos,
    arch: Arch::X86_64,
};
const MAC_ARM64: Platform = Platform {
    os: Os::Macos,
    arch: Arch::Aarch64,
};
const WIN_X64: Platform = Platform {
    os: Os::Windows,
    arch: Arch::X86_64,
};
const WIN_ARM64: Platform = Platform {
    os: Os::Windows,
    arch: Arch::Aarch64,
};

const ALL: [Platform; 6] = [
    LINUX_X64,
    LINUX_ARM64,
    MAC_X64,
    MAC_ARM64,
    WIN_X64,
    WIN_ARM64,
];

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn fixture(name: &str) -> PackageMetadata {
    let text = std::fs::read_to_string(fixture_path(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn on(name: &str, platform: Platform) -> PackageAnalysis {
    fixture(name).analyze(&platform)
}

/// Installs from a wheel on `platform`, over exactly `range`.
#[track_caller]
fn assert_wheels(a: &PackageAnalysis, platform: Platform, range: &str) {
    let here = platform.label();
    assert!(
        a.has_platform_wheels,
        "{} should have wheels on {here}",
        a.name
    );
    assert!(!a.no_files_for_platform, "{} on {here}", a.name);
    assert_eq!(a.supported.to_range_string(), range, "{} on {here}", a.name);
}

/// Nothing at all installs on `platform`: foreign wheels and no sdist.
#[track_caller]
fn assert_nothing_installs(a: &PackageAnalysis, platform: Platform) {
    let here = platform.label();
    assert!(a.no_files_for_platform, "{} on {here}", a.name);
    assert!(a.supported.is_empty(), "{} on {here}", a.name);
    assert!(
        a.no_files_message(here).is_some(),
        "{} on {here} needs an explanation",
        a.name
    );
}

/// No wheel here, but an sdist and other platforms' wheels: a native build.
#[track_caller]
fn assert_native_source_build(a: &PackageAnalysis, platform: Platform) {
    let here = platform.label();
    assert!(!a.has_platform_wheels, "{} on {here}", a.name);
    assert_eq!(
        a.sdist_kind(),
        Some(SdistKind::Native),
        "{} on {here}",
        a.name
    );
}

// ---------------------------------------------------------------------------
// Tag matching, platform by platform.
// ---------------------------------------------------------------------------

/// Every platform tag seen in the recorded fixtures, against every platform.
/// One row per tag: Linux x64, Linux arm64, macOS x64, macOS arm64, Windows
/// x64, Windows arm64.
#[test]
fn wheel_platform_tags_match_only_their_own_platform() {
    #[rustfmt::skip]
    let table: &[(&str, [bool; 6])] = &[
        ("any",                        [true,  true,  true,  true,  true,  true ]),
        ("manylinux_2_28_x86_64",      [true,  false, false, false, false, false]),
        ("manylinux2014_x86_64",       [true,  false, false, false, false, false]),
        ("manylinux1_x86_64",          [true,  false, false, false, false, false]),
        ("manylinux_2_28_aarch64",     [false, true,  false, false, false, false]),
        ("manylinux2014_aarch64",      [false, true,  false, false, false, false]),
        ("manylinux_2_28_ppc64le",     [false, false, false, false, false, false]),
        ("manylinux_2_31_armv7l",      [false, false, false, false, false, false]),
        ("macosx_10_9_x86_64",         [false, false, true,  false, false, false]),
        ("macosx_14_0_x86_64",         [false, false, true,  false, false, false]),
        ("macosx_11_0_arm64",          [false, false, false, true,  false, false]),
        ("macosx_14_0_arm64",          [false, false, false, true,  false, false]),
        ("macosx_10_9_universal2",     [false, false, true,  true,  false, false]),
        ("macosx_10_15_universal2",    [false, false, true,  true,  false, false]),
        ("win_amd64",                  [false, false, false, false, true,  false]),
        ("win_arm64",                  [false, false, false, false, false, true ]),
        ("win32",                      [false, false, false, false, false, false]),
    ];
    for (tag, expected) in table {
        for (platform, want) in ALL.iter().zip(expected) {
            assert_eq!(
                platform.wheel_tag_matches(tag),
                *want,
                "{tag} on {}",
                platform.label()
            );
        }
    }
}

/// The engine does not know the host's macOS release, so a wheel's deployment
/// target is not checked: `macosx_14_0_arm64` counts on any Apple Silicon Mac.
/// This pins the documented, deliberately permissive behaviour.
#[test]
fn macos_deployment_target_is_not_a_filter() {
    let a = on("torch-2.15.0.json", MAC_ARM64);
    assert_wheels(&a, MAC_ARM64, "3.11–3.15");

    // numpy publishes both 11.0 and 14.0 arm64 builds; either counts.
    let numpy = fixture("numpy-2.5.4.json");
    let only_14: Vec<_> = numpy
        .urls
        .iter()
        .filter(|f| !f.filename.contains("macosx_11_0"))
        .cloned()
        .collect();
    let mut trimmed = numpy.clone();
    trimmed.urls = only_14;
    assert_eq!(
        trimmed.analyze(&MAC_ARM64).supported,
        numpy.analyze(&MAC_ARM64).supported
    );
}

// ---------------------------------------------------------------------------
// Real releases whose wheel sets differ between platforms.
// ---------------------------------------------------------------------------

/// pywin32 ships Windows wheels only and no sdist.
#[test]
fn pywin32_installs_only_on_windows() {
    for p in [WIN_X64, WIN_ARM64] {
        assert_wheels(&on("pywin32-312.json", p), p, "3.9–3.15");
    }
    for p in [LINUX_X64, LINUX_ARM64, MAC_X64, MAC_ARM64] {
        let a = on("pywin32-312.json", p);
        assert_nothing_installs(&a, p);
        assert_eq!(
            a.wheel_platforms,
            ["Windows arm64", "Windows x64", "Windows x86"]
        );
    }
}

/// uvloop has no Windows wheels at all; its sdist means a native build there.
#[test]
fn uvloop_is_a_source_build_on_windows() {
    for p in [LINUX_X64, LINUX_ARM64, MAC_X64, MAC_ARM64] {
        assert_wheels(&on("uvloop-0.23.0.json", p), p, "3.8–3.15");
    }
    for p in [WIN_X64, WIN_ARM64] {
        assert_native_source_build(&on("uvloop-0.23.0.json", p), p);
    }
}

/// pyobjc-core is macOS-only and every wheel is universal2, so both Mac
/// architectures are covered by the same files.
#[test]
fn pyobjc_core_universal2_covers_both_macs() {
    for p in [MAC_X64, MAC_ARM64] {
        assert_wheels(&on("pyobjc-core-12.2.2.json", p), p, "3.10–3.15");
    }
    for p in [LINUX_X64, LINUX_ARM64, WIN_X64, WIN_ARM64] {
        assert_native_source_build(&on("pyobjc-core-12.2.2.json", p), p);
    }
}

/// tensorflow 2.21 has no macOS x64 and no Windows arm64 wheels, and no sdist.
#[test]
fn tensorflow_2_21_platform_gaps() {
    for p in [LINUX_X64, LINUX_ARM64, MAC_ARM64, WIN_X64] {
        assert_wheels(&on("tensorflow-2.21.0.json", p), p, "3.10–3.13");
    }
    for p in [MAC_X64, WIN_ARM64] {
        assert_nothing_installs(&on("tensorflow-2.21.0.json", p), p);
    }
    assert_eq!(
        on("tensorflow-2.21.0.json", WIN_ARM64)
            .no_files_message(WIN_ARM64.label())
            .unwrap(),
        "tensorflow 2.21.0 publishes no files for Windows arm64 (only Linux arm64, Linux x64, Windows x64, macOS wheels, no source distribution), so it cannot be installed here."
    );
}

/// Before 2.13 Apple Silicon took tensorflow-macos: 2.12 has no arm64 Mac
/// wheel, while Intel Macs still had one.
#[test]
fn tensorflow_2_12_has_no_apple_silicon_wheel() {
    assert_wheels(&on("tensorflow-2.12.0.json", MAC_X64), MAC_X64, "3.8–3.11");
    assert_nothing_installs(&on("tensorflow-2.12.0.json", MAC_ARM64), MAC_ARM64);
}

/// onnxruntime: no Intel Mac build; everywhere else 3.11–3.14.
#[test]
fn onnxruntime_has_no_intel_mac_build() {
    for p in [LINUX_X64, LINUX_ARM64, MAC_ARM64, WIN_X64, WIN_ARM64] {
        assert_wheels(&on("onnxruntime-1.31.1.json", p), p, "3.11–3.14");
    }
    assert_nothing_installs(&on("onnxruntime-1.31.1.json", MAC_X64), MAC_X64);
}

/// mediapipe 1.2.0's wheels are `py3-none-<platform>`: any CPython 3 where a
/// wheel exists, nothing on an Intel Mac.
#[test]
fn mediapipe_py3_platform_wheels() {
    for p in [LINUX_X64, LINUX_ARM64, MAC_ARM64, WIN_X64, WIN_ARM64] {
        assert_wheels(&on("mediapipe-1.2.0.json", p), p, "3.7–3.15");
    }
    assert_nothing_installs(&on("mediapipe-1.2.0.json", MAC_X64), MAC_X64);
}

/// numpy covers all six platforms over the same range, whatever mix of
/// deployment targets and manylinux/musllinux it uses to get there.
#[test]
fn numpy_covers_every_platform() {
    for p in ALL {
        assert_wheels(&on("numpy-2.5.4.json", p), p, "3.12–3.15");
    }
}

/// cryptography ships stable-ABI wheels: `cp39-abi3` covers 3.9 and every
/// newer minor. Its only Mac wheels are arm64 and it has no Windows arm64
/// wheel, so those two platforms are native builds.
#[test]
fn cryptography_abi3_per_platform() {
    for p in [LINUX_X64, LINUX_ARM64, MAC_ARM64, WIN_X64] {
        let a = on("cryptography-50.0.2.json", p);
        assert!(a.has_platform_wheels, "{}", p.label());
        for minor in 9..=15 {
            assert!(
                a.supported.contains(PyVersion::py3(minor)),
                "3.{minor} on {}",
                p.label()
            );
        }
    }
    for p in [MAC_X64, WIN_ARM64] {
        assert_native_source_build(&on("cryptography-50.0.2.json", p), p);
    }
}

// ---------------------------------------------------------------------------
// torch: what PyPI carries per platform, and pins.
// ---------------------------------------------------------------------------

fn torch_source() -> FixtureSource {
    let mut src = FixtureSource::new();
    src.load_json("torch", fixture_path("torch-2.15.0.json"))
        .unwrap();
    src.load_json("torch==2.2.2", fixture_path("torch-2.2.2.json"))
        .unwrap();
    src
}

/// PyPI's torch has dropped Intel Macs (2.2 was the last) and has never
/// carried Windows arm64 (that build lives on download.pytorch.org/whl/cpu,
/// which this analysis does not read).
#[tokio::test]
async fn torch_on_pypi_per_platform() {
    let src = torch_source();
    for p in [LINUX_X64, LINUX_ARM64, MAC_ARM64, WIN_X64] {
        let report = analyze(&src, p, &[Requirement::any("torch")]).await;
        assert_eq!(
            report.intersection.to_range_string(),
            "3.11–3.15",
            "{}",
            p.label()
        );
    }
    for p in [MAC_X64, WIN_ARM64] {
        let report = analyze(&src, p, &[Requirement::any("torch")]).await;
        let names: Vec<_> = report.uninstallable().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["torch"], "{}", p.label());
    }
}

/// On an Intel Mac, pinning the last release with x86_64 wheels works.
#[tokio::test]
async fn torch_pinned_to_2_2_installs_on_an_intel_mac() {
    let pin = Requirement {
        name: "torch".into(),
        spec: "==2.2.2".into(),
    };
    let report = analyze(&torch_source(), MAC_X64, &[pin]).await;
    assert_eq!(report.per_package[0].version, "2.2.2");
    assert_eq!(report.intersection.to_range_string(), "3.8–3.12");
    assert_eq!(report.suggested_python(), Some(PyVersion::py3(12)));
}

/// A project mixing a Windows-only and a Unix-only dependency: each platform
/// names the one that does not install there, never a version conflict.
#[tokio::test]
async fn windows_only_and_unix_only_dependencies() {
    let mut src = FixtureSource::new();
    src.load_json("pywin32", fixture_path("pywin32-312.json"))
        .unwrap();
    src.load_json("tensorflow", fixture_path("tensorflow-2.21.0.json"))
        .unwrap();
    let reqs = [Requirement::any("pywin32"), Requirement::any("tensorflow")];

    let linux = analyze(&src, LINUX_X64, &reqs).await;
    assert!(linux.conflicts.is_empty());
    let names: Vec<_> = linux.uninstallable().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["pywin32"]);

    let win = analyze(&src, WIN_X64, &reqs).await;
    assert_eq!(win.intersection.to_range_string(), "3.10–3.13");

    let win_arm = analyze(&src, WIN_ARM64, &reqs).await;
    let names: Vec<_> = win_arm.uninstallable().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["tensorflow"]);
}

/// An unlabelled torch version (`2.5.1`, no `+cu121`) is whatever PyPI's
/// default wheel is on that platform: CUDA on Linux x64, CPU on Windows, and
/// not guessed elsewhere (Linux arm64's default build has changed over time,
/// macOS uses MPS).
#[test]
fn unlabelled_torch_build_depends_on_the_platform() {
    let expected = [
        (LINUX_X64, TorchBuild::Cuda(None)),
        (LINUX_ARM64, TorchBuild::Unknown),
        (MAC_X64, TorchBuild::Unknown),
        (MAC_ARM64, TorchBuild::Unknown),
        (WIN_X64, TorchBuild::Cpu),
        (WIN_ARM64, TorchBuild::Cpu),
    ];
    for (p, build) in expected {
        assert_eq!(
            classify("2.5.1", None, p.os, p.arch),
            build,
            "{}",
            p.label()
        );
        // A local label is authoritative everywhere.
        assert_eq!(classify("2.5.1+cpu", None, p.os, p.arch), TorchBuild::Cpu);
        assert_eq!(
            classify("2.5.1+cu121", None, p.os, p.arch),
            TorchBuild::Cuda(None)
        );
    }
}

/// A `win32` wheel is a 32-bit build and does not install into the 64-bit
/// CPython on Windows x64. Taken from numpy 1.26.4's real file names, with
/// the win_amd64 cp312 wheel removed: the win32 one must not stand in for it.
#[test]
fn win32_wheels_do_not_count_on_64_bit_windows() {
    let mut meta = fixture("numpy-1.26.4.json");
    meta.urls.retain(|f| {
        f.filename.contains("win32")
            || f.filename.contains("win_amd64") && !f.filename.contains("cp312")
    });
    let a = meta.analyze(&WIN_X64);
    assert_eq!(a.supported.to_range_string(), "3.9–3.11");

    // Only win32 wheels: nothing installs, and the message says why.
    meta.urls.retain(|f| f.filename.contains("win32"));
    let a = meta.analyze(&WIN_X64);
    assert_nothing_installs(&a, WIN_X64);
    assert_eq!(a.wheel_platforms, ["Windows x86"]);
}
