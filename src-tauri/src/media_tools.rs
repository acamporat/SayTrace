use std::{
    env,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use crate::{
    error::{CoreError, CoreResult},
    layout::AppLayout,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaTool {
    Ffmpeg,
    Ffprobe,
}

impl MediaTool {
    fn executable_name(self) -> &'static str {
        match self {
            Self::Ffmpeg => {
                if cfg!(windows) {
                    "ffmpeg.exe"
                } else {
                    "ffmpeg"
                }
            }
            Self::Ffprobe => {
                if cfg!(windows) {
                    "ffprobe.exe"
                } else {
                    "ffprobe"
                }
            }
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Ffmpeg => "FFmpeg",
            Self::Ffprobe => "FFprobe",
        }
    }
}

/// Resolve a media executable from the installer-owned processing runtime.
///
/// Debug builds may fall back to an explicitly installed development tool on
/// PATH. Release builds never execute a same-named binary from PATH.
pub fn resolve(layout: &AppLayout, tool: MediaTool) -> CoreResult<PathBuf> {
    let development_path = cfg!(debug_assertions)
        .then(|| env::var_os("PATH"))
        .flatten();
    resolve_with_development_path(layout.runtime(), tool, development_path.as_deref())
}

fn resolve_with_development_path(
    runtime: &Path,
    tool: MediaTool,
    development_path: Option<&OsStr>,
) -> CoreResult<PathBuf> {
    let executable = tool.executable_name();
    let canonical_runtime = runtime.canonicalize()?;
    for candidate in [
        runtime.join(executable),
        runtime.join("bin").join(executable),
        runtime.join("ffmpeg").join("bin").join(executable),
    ] {
        if !candidate.is_file() {
            continue;
        }
        let canonical = candidate.canonicalize()?;
        if canonical.starts_with(&canonical_runtime) {
            return Ok(canonical);
        }
    }

    if let Some(path) = development_path {
        for directory in env::split_paths(path) {
            let candidate = directory.join(executable);
            if candidate.is_file() {
                return Ok(candidate.canonicalize()?);
            }
        }
    }

    Err(CoreError::MediaToolMissing(format!(
        "{} is not installed in the SayTrace runtime{}",
        tool.display_name(),
        if development_path.is_some() {
            " or on the development PATH"
        } else {
            ""
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, fs};

    fn executable(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test executable").unwrap();
    }

    fn joined_path(paths: &[&Path]) -> OsString {
        env::join_paths(paths).unwrap()
    }

    #[test]
    fn packaged_runtime_wins_over_development_path() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let development = temp.path().join("development");
        fs::create_dir_all(&runtime).unwrap();
        let packaged = runtime.join(MediaTool::Ffmpeg.executable_name());
        let fallback = development.join(MediaTool::Ffmpeg.executable_name());
        executable(&packaged);
        executable(&fallback);
        let path = joined_path(&[&development]);

        let resolved =
            resolve_with_development_path(&runtime, MediaTool::Ffmpeg, Some(&path)).unwrap();

        assert_eq!(resolved, packaged.canonicalize().unwrap());
    }

    #[test]
    fn development_path_is_used_only_when_explicitly_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let development = temp.path().join("development");
        fs::create_dir_all(&runtime).unwrap();
        let fallback = development.join(MediaTool::Ffprobe.executable_name());
        executable(&fallback);
        let path = joined_path(&[&development]);

        let resolved =
            resolve_with_development_path(&runtime, MediaTool::Ffprobe, Some(&path)).unwrap();

        assert_eq!(resolved, fallback.canonicalize().unwrap());
    }

    #[test]
    fn missing_tool_fails_closed_without_development_path() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        fs::create_dir_all(&runtime).unwrap();

        let error = resolve_with_development_path(&runtime, MediaTool::Ffmpeg, None).unwrap_err();

        assert!(matches!(error, CoreError::MediaToolMissing(_)));
    }
}
