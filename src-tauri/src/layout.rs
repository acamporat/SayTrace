use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use crate::error::{CoreError, CoreResult};

#[derive(Debug, Clone)]
pub struct AppLayout {
    root: PathBuf,
    database: PathBuf,
    library: PathBuf,
    media: PathBuf,
    recordings: PathBuf,
    artifacts: PathBuf,
    work: PathBuf,
    exports: PathBuf,
    backups: PathBuf,
    models: PathBuf,
    runtime: PathBuf,
    cache: PathBuf,
    logs: PathBuf,
    temp: PathBuf,
}

impl AppLayout {
    pub fn create(root: impl Into<PathBuf>) -> CoreResult<Self> {
        Self::create_with_runtime(root, None)
    }

    pub fn create_with_runtime(
        root: impl Into<PathBuf>,
        bundled_runtime: Option<PathBuf>,
    ) -> CoreResult<Self> {
        let root = root.into();
        let runtime = bundled_runtime.unwrap_or_else(|| root.join("runtime"));
        let layout = Self {
            database: root.join("local-transcript.sqlite3"),
            library: root.join("library"),
            media: root.join("library").join("media"),
            recordings: root.join("library").join("recordings"),
            artifacts: root.join("library").join("artifacts"),
            work: root.join("library").join("work"),
            exports: root.join("exports"),
            backups: root.join("backups"),
            models: root.join("models"),
            runtime,
            cache: root.join("cache"),
            logs: root.join("logs"),
            temp: root.join("temp"),
            root,
        };
        for directory in [
            &layout.root,
            &layout.library,
            &layout.media,
            &layout.recordings,
            &layout.artifacts,
            &layout.work,
            &layout.exports,
            &layout.backups,
            &layout.models,
            &layout.cache,
            &layout.logs,
            &layout.temp,
        ] {
            fs::create_dir_all(directory)?;
        }
        if layout.runtime.starts_with(&layout.root) {
            fs::create_dir_all(&layout.runtime)?;
        } else if !runtime_payload_ready(&layout.runtime) {
            return Err(CoreError::InvalidInput(
                "bundled processing runtime is incomplete".into(),
            ));
        }
        Ok(layout)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn database(&self) -> &Path {
        &self.database
    }

    pub fn library(&self) -> &Path {
        &self.library
    }

    pub fn media(&self) -> &Path {
        &self.media
    }

    pub fn recordings(&self) -> &Path {
        &self.recordings
    }

    pub fn artifacts(&self) -> &Path {
        &self.artifacts
    }

    pub fn work(&self) -> &Path {
        &self.work
    }

    pub fn exports(&self) -> &Path {
        &self.exports
    }

    pub fn backups(&self) -> &Path {
        &self.backups
    }

    pub fn models(&self) -> &Path {
        &self.models
    }

    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    pub fn temp(&self) -> &Path {
        &self.temp
    }

    pub fn cache(&self) -> &Path {
        &self.cache
    }

    pub fn relative_to_root(&self, path: &Path) -> CoreResult<String> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| CoreError::Security("path is outside the application data root".into()))?;
        Ok(relative.to_string_lossy().replace('\\', "/"))
    }

    pub fn resolve_relative(&self, relative: &str) -> CoreResult<PathBuf> {
        let path = Path::new(relative);
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            return Err(CoreError::Security(
                "stored asset path contains unsafe components".into(),
            ));
        }
        let joined = self.root.join(path);
        let root = self.root.canonicalize()?;
        let canonical = joined.canonicalize()?;
        if !canonical.starts_with(root) {
            return Err(CoreError::Security(
                "stored asset resolves outside the application data root".into(),
            ));
        }
        Ok(canonical)
    }
}

pub(crate) fn runtime_payload_ready(runtime: &Path) -> bool {
    [
        "local-transcript-worker.exe",
        "ffmpeg.exe",
        "ffprobe.exe",
        "runtime-manifest.json",
    ]
    .iter()
    .all(|name| runtime.join(name).is_file())
}

/// Returns the logical size of ordinary files beneath a managed directory.
///
/// Directory symlinks and Windows reparse points are deliberately not
/// traversed. File links/reparse points are skipped as well, so storage
/// accounting cannot escape the managed library.
pub(crate) fn managed_directory_size(root: &Path) -> CoreResult<u64> {
    let root = root.canonicalize()?;
    let metadata = fs::symlink_metadata(&root)?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(CoreError::Security(
            "managed storage root is not an ordinary directory".into(),
        ));
    }

    fn visit(directory: &Path) -> CoreResult<u64> {
        let mut total = 0_u64;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                // Active workers publish files with atomic renames. If an
                // entry disappears between read_dir and metadata, omit that
                // transient path from this snapshot instead of failing stats.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if is_link_or_reparse(&metadata) {
                continue;
            }
            if metadata.is_dir() {
                total = total.saturating_add(visit(&path)?);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
        Ok(total)
    }

    visit(&root)
}

/// Creates or opens exactly one ordinary direct child directory inside a
/// managed parent and returns its canonical path.
pub(crate) fn managed_child_directory(
    managed_root: &Path,
    parent: &Path,
    child_name: &str,
) -> CoreResult<PathBuf> {
    validate_child_name(child_name)?;
    let (canonical_root, canonical_parent) = canonical_managed_parent(managed_root, parent)?;
    debug_assert!(canonical_parent.starts_with(canonical_root));
    let candidate = parent.join(child_name);
    match fs::create_dir(&candidate) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(&candidate)?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err(CoreError::Security(
            "managed child directory is not an ordinary directory".into(),
        ));
    }
    let canonical_candidate = candidate.canonicalize()?;
    if canonical_candidate.parent() != Some(canonical_parent.as_path()) {
        return Err(CoreError::Security(
            "managed child directory is not an exact direct child".into(),
        ));
    }
    Ok(canonical_candidate)
}

/// Removes exactly one direct child directory of a parent inside a managed
/// root. Nested links/reparse points are unlinked and never traversed.
pub(crate) fn remove_managed_child_tree(
    managed_root: &Path,
    parent: &Path,
    child_name: &str,
) -> CoreResult<()> {
    validate_child_name(child_name)?;
    let (_, canonical_parent) = canonical_managed_parent(managed_root, parent)?;
    let candidate = parent.join(child_name);
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if is_link_or_reparse(&metadata) {
        return Err(CoreError::Security(
            "managed child directory is a link or reparse point".into(),
        ));
    }
    if !metadata.is_dir() {
        return Err(CoreError::Security(
            "managed child path is not a directory".into(),
        ));
    }
    let canonical_candidate = candidate.canonicalize()?;
    if canonical_candidate.parent() != Some(canonical_parent.as_path()) {
        return Err(CoreError::Security(
            "managed child directory is not an exact direct child".into(),
        ));
    }

    remove_tree_without_following_links(&canonical_candidate)
}

fn validate_child_name(child_name: &str) -> CoreResult<()> {
    let child = Path::new(child_name);
    if child.components().count() != 1
        || !matches!(child.components().next(), Some(Component::Normal(_)))
    {
        return Err(CoreError::Security(
            "managed child name contains unsafe path components".into(),
        ));
    }
    Ok(())
}

fn canonical_managed_parent(managed_root: &Path, parent: &Path) -> CoreResult<(PathBuf, PathBuf)> {
    let canonical_root = managed_root.canonicalize()?;
    let canonical_parent = parent.canonicalize()?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(CoreError::Security(
            "managed directory parent resolves outside its root".into(),
        ));
    }
    let parent_metadata = fs::symlink_metadata(parent)?;
    if !parent_metadata.is_dir() || is_link_or_reparse(&parent_metadata) {
        return Err(CoreError::Security(
            "managed directory parent is not an ordinary directory".into(),
        ));
    }
    Ok((canonical_root, canonical_parent))
}

fn remove_tree_without_following_links(directory: &Path) -> CoreResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if is_link_or_reparse(&metadata) {
            remove_link_or_reparse(&path)?;
        } else if metadata.is_dir() {
            remove_tree_without_following_links(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    fs::remove_dir(directory)?;
    Ok(())
}

fn remove_link_or_reparse(path: &Path) -> CoreResult<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(file_error) => match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(_) => Err(file_error.into()),
        },
    }
}

fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        return metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let layout = AppLayout::create(temp.path()).unwrap();
        assert!(layout.resolve_relative("../secret").is_err());
        assert!(layout.resolve_relative("C:/Windows/notepad.exe").is_err());
    }

    #[test]
    fn uses_complete_bundled_runtime_without_copying_it_into_user_data() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("installed-resources").join("runtime");
        fs::create_dir_all(&bundled).unwrap();
        for name in [
            "local-transcript-worker.exe",
            "ffmpeg.exe",
            "ffprobe.exe",
            "runtime-manifest.json",
        ] {
            fs::write(bundled.join(name), b"runtime").unwrap();
        }

        let layout =
            AppLayout::create_with_runtime(temp.path().join("app-data"), Some(bundled.clone()))
                .unwrap();

        assert_eq!(layout.runtime(), bundled);
        assert!(!layout.root().join("runtime").exists());
    }

    #[test]
    fn rejects_incomplete_bundled_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("runtime");
        fs::create_dir_all(&bundled).unwrap();
        fs::write(bundled.join("ffmpeg.exe"), b"runtime").unwrap();

        let result = AppLayout::create_with_runtime(temp.path().join("app-data"), Some(bundled));

        assert!(result.is_err());
    }

    #[test]
    fn managed_size_counts_files_without_following_external_links() {
        let temp = tempfile::tempdir().unwrap();
        let layout = AppLayout::create(temp.path().join("app")).unwrap();
        fs::write(layout.media().join("media.bin"), vec![0_u8; 11]).unwrap();
        fs::write(layout.recordings().join("recording.bin"), vec![0_u8; 13]).unwrap();
        fs::write(layout.artifacts().join("artifact.bin"), vec![0_u8; 17]).unwrap();
        fs::write(layout.work().join("active.bin"), vec![0_u8; 19]).unwrap();

        let outside = temp.path().join("outside.bin");
        fs::write(&outside, vec![0_u8; 101]).unwrap();
        let linked = layout.media().join("external-link.bin");
        create_file_symlink(&outside, &linked);

        assert_eq!(managed_directory_size(layout.library()).unwrap(), 60);
    }

    #[test]
    fn managed_child_cleanup_is_exact_and_does_not_follow_links() {
        let temp = tempfile::tempdir().unwrap();
        let layout = AppLayout::create(temp.path().join("app")).unwrap();
        let job_id = "019fa9ab-d5f4-7431-b3e9-7dcf423d1ebb";
        let workspace = layout.work().join(job_id);
        fs::create_dir_all(workspace.join("nested")).unwrap();
        fs::write(workspace.join("nested").join("scratch.bin"), b"scratch").unwrap();

        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("keep.bin"), b"keep").unwrap();
        let linked = workspace.join("external-link");
        create_directory_symlink(&outside, &linked);

        remove_managed_child_tree(layout.library(), layout.work(), job_id).unwrap();
        assert!(!workspace.exists());
        assert_eq!(fs::read(outside.join("keep.bin")).unwrap(), b"keep");
        assert!(remove_managed_child_tree(layout.library(), layout.work(), "../outside").is_err());
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) {
        let _ = std::os::windows::fs::symlink_file(target, link);
    }

    #[cfg(not(windows))]
    fn create_file_symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_directory_symlink(target: &Path, link: &Path) {
        let _ = std::os::windows::fs::symlink_dir(target, link);
    }

    #[cfg(not(windows))]
    fn create_directory_symlink(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}
