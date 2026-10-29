//! Platform detection (OS/arch) and managed directories.
//!
//! Used by the wheel-tag filter (which wheels are installable *here*), the uv
//! downloader (which standalone archive to fetch), and the cache/data-dir logic.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// The platform this binary is running on.
    pub fn current() -> Platform {
        let os = if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        };
        let arch = if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        };
        Platform { os, arch }
    }

    /// Does a single wheel platform tag run on this machine?
    ///
    /// `any` (pure-python) always matches. Otherwise we match the tag family and
    /// architecture. This is intentionally permissive on the manylinux/macos
    /// *version* component — a `manylinux_2_17_x86_64` wheel runs on essentially
    /// every modern x86-64 Linux, and getting the glibc floor exactly right is
    /// out of scope for interpreter-version reasoning.
    pub fn wheel_tag_matches(&self, tag: &str) -> bool {
        if tag == "any" {
            return true;
        }
        match self.os {
            // `win32` is the 32-bit x86 build: it does not load into the
            // 64-bit CPython uv and the python.org installer put on x64.
            Os::Windows => match self.arch {
                Arch::X86_64 => tag == "win_amd64",
                Arch::Aarch64 => tag == "win_arm64",
            },
            Os::Macos => {
                if !tag.starts_with("macosx") {
                    return false;
                }
                match self.arch {
                    // Universal2 wheels serve both arches.
                    Arch::Aarch64 => tag.ends_with("arm64") || tag.ends_with("universal2"),
                    Arch::X86_64 => {
                        tag.ends_with("x86_64")
                            || tag.ends_with("universal2")
                            || tag.ends_with("intel")
                    }
                }
            }
            Os::Linux => {
                let is_linux = tag.starts_with("manylinux") || tag.starts_with("musllinux");
                if !is_linux {
                    return false;
                }
                match self.arch {
                    Arch::X86_64 => tag.ends_with("x86_64"),
                    Arch::Aarch64 => tag.ends_with("aarch64"),
                }
            }
        }
    }

    /// Human name for messages, e.g. `Windows x64`, `macOS arm64`.
    pub fn label(&self) -> &'static str {
        match (self.os, self.arch) {
            (Os::Linux, Arch::X86_64) => "Linux x64",
            (Os::Linux, Arch::Aarch64) => "Linux arm64",
            (Os::Macos, Arch::X86_64) => "macOS x64",
            (Os::Macos, Arch::Aarch64) => "macOS arm64",
            (Os::Windows, Arch::X86_64) => "Windows x64",
            (Os::Windows, Arch::Aarch64) => "Windows arm64",
        }
    }

    /// Target triple used by uv's standalone release asset names.
    pub fn uv_triple(&self) -> &'static str {
        match (self.os, self.arch) {
            (Os::Linux, Arch::X86_64) => "x86_64-unknown-linux-gnu",
            (Os::Linux, Arch::Aarch64) => "aarch64-unknown-linux-gnu",
            (Os::Macos, Arch::X86_64) => "x86_64-apple-darwin",
            (Os::Macos, Arch::Aarch64) => "aarch64-apple-darwin",
            (Os::Windows, Arch::X86_64) => "x86_64-pc-windows-msvc",
            (Os::Windows, Arch::Aarch64) => "aarch64-pc-windows-msvc",
        }
    }
}

/// Managed data dir (downloaded uv lives here). Never touches system dirs.
pub fn data_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "PyPilot", "pypilot")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("pypilot"))
}

/// Platform cache dir (PyPI metadata cache lives here).
pub fn cache_dir() -> PathBuf {
    directories::ProjectDirs::from("dev", "PyPilot", "pypilot")
        .map(|d| d.cache_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("pypilot-cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_wheel_matches_everywhere() {
        let p = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert!(p.wheel_tag_matches("any"));
    }

    #[test]
    fn linux_x64_matches_manylinux() {
        let p = Platform {
            os: Os::Linux,
            arch: Arch::X86_64,
        };
        assert!(p.wheel_tag_matches("manylinux_2_17_x86_64"));
        assert!(p.wheel_tag_matches("manylinux2014_x86_64"));
        assert!(!p.wheel_tag_matches("win_amd64"));
        assert!(!p.wheel_tag_matches("macosx_11_0_arm64"));
    }

    #[test]
    fn mac_arm_matches_arm64_and_universal() {
        let p = Platform {
            os: Os::Macos,
            arch: Arch::Aarch64,
        };
        assert!(p.wheel_tag_matches("macosx_11_0_arm64"));
        assert!(p.wheel_tag_matches("macosx_10_9_universal2"));
        assert!(!p.wheel_tag_matches("macosx_10_9_x86_64"));
    }

    #[test]
    fn windows_x64_matches_win_amd64() {
        let p = Platform {
            os: Os::Windows,
            arch: Arch::X86_64,
        };
        assert!(p.wheel_tag_matches("win_amd64"));
        assert!(!p.wheel_tag_matches("win32"));
        assert!(!p.wheel_tag_matches("manylinux_2_17_x86_64"));
    }
}
