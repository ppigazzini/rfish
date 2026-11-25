//! The shared upstream oracle, reached through the one script every port carries.
//!
//! `tools/upstream/upstream_oracle.sh` is the same file in every port and owns where a build
//! lives, how it is made and when it counts as finished. Nothing here spells a path of its
//! own: a second copy of that knowledge is a second oracle.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{capture, workspace_root};

/// Which build of the pinned commit to ask for.
pub(crate) enum Variant<'a> {
    /// The default build, which the correctness gates compare against.
    Reference,
    /// clang, PGO on top of LTO, at a named `ARCH`: the other side of a COST comparison.
    Cost { arch: &'a str, patch: Option<&'a Path> },
}

fn script(variant: &Variant) -> Command {
    let mut cmd = Command::new("bash");
    cmd.current_dir(workspace_root())
        .arg(workspace_root().join("tools/upstream/upstream_oracle.sh"));
    if let Variant::Cost { arch, patch } = variant {
        cmd.env("ORACLE_COMP", "clang")
            .env("ORACLE_TARGET", "profile-build")
            .env("ORACLE_ARCH", arch);
        if let Some(patch) = patch {
            cmd.arg("--patch").arg(patch);
        }
    }
    cmd
}

/// The binary at the pin if it is already built, and `None` on any host that has none.
///
/// Never builds, so a gate inside `parity` can ask. The key carries the commit, so a build
/// left from another pin is a different directory and cannot answer.
pub(crate) fn find(variant: &Variant) -> Option<PathBuf> {
    let out = script(variant).arg("--path").stderr(Stdio::null()).output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (out.status.success() && path.is_file()).then_some(path)
}

/// The binary at the pin, built if it is absent.
pub(crate) fn build(variant: &Variant) -> Result<PathBuf, String> {
    Ok(PathBuf::from(capture(&mut script(variant))?.trim()))
}
