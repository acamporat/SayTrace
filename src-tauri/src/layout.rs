use std::{
    collections::{BTreeSet, HashSet},
    fs,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use parking_lot::{Condvar, Mutex};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    error::{CoreError, CoreResult},
    performance::PerformanceSpan,
};

const RUNTIME_MANIFEST_NAME: &str = "runtime-manifest.json";
const RUNTIME_MANIFEST_MAX_BYTES: u64 = 8 * 1024 * 1024;
const RUNTIME_MANIFEST_SCHEMA_VERSION: u32 = 1;
const RUNTIME_PRODUCT: &str = "SayTrace Runtime";
const APP_IDENTIFIER: &str = "com.localtranscript.desktop";
const WORKER_PROTOCOL_VERSION: &str = "1.0";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const RUNTIME_PIPELINE_VERSION: &str = "2026.08.13.1";
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const RUNTIME_PIPELINE_VERSION: &str = "2026.07.28.1";
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const EMBEDDED_MODEL_MANIFEST: &[u8] = include_bytes!("../../worker/model-manifest.macos.json");
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
const EMBEDDED_MODEL_MANIFEST: &[u8] = include_bytes!("../../worker/model-manifest.json");

#[derive(Debug, Deserialize)]
struct RuntimeManifest {
    schema_version: u32,
    product: String,
    app_identifier: String,
    runtime_version: String,
    variant: String,
    architecture: String,
    worker_protocol_version: String,
    pipeline_version: String,
    model_manifest: RuntimeModelManifest,
    payload: Vec<RuntimePayloadRecord>,
}

#[derive(Debug, Deserialize)]
struct RuntimeModelManifest {
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct RuntimePayloadRecord {
    path: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Clone, Copy)]
struct RuntimePlatform {
    architecture: &'static str,
    variants: &'static [&'static str],
    executable_names: &'static [&'static str],
}

#[derive(Debug, Clone)]
enum RuntimeValidationState {
    Unavailable,
    Pending,
    Validating,
    Valid,
    Invalid(String),
}

#[derive(Debug)]
struct RuntimeValidation {
    state: Mutex<RuntimeValidationState>,
    changed: Condvar,
}

impl RuntimeValidation {
    fn new(state: RuntimeValidationState) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppLayout {
    root: PathBuf,
    canonical_root: PathBuf,
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
    runtime_validation: Arc<RuntimeValidation>,
    cache: PathBuf,
    logs: PathBuf,
    temp: PathBuf,
}

impl AppLayout {
    pub fn create(root: impl Into<PathBuf>) -> CoreResult<Self> {
        Self::create_with_runtime_mode(root, None, false)
    }

    pub fn create_with_runtime(
        root: impl Into<PathBuf>,
        bundled_runtime: Option<PathBuf>,
    ) -> CoreResult<Self> {
        Self::create_with_runtime_mode(root, bundled_runtime, false)
    }

    /// Creates the app-data layout without hashing the immutable bundled
    /// runtime on the UI startup path. `ensure_runtime_validated` still performs
    /// the exact full validation before any runtime executable can be selected.
    pub(crate) fn create_with_deferred_runtime(
        root: impl Into<PathBuf>,
        bundled_runtime: PathBuf,
    ) -> CoreResult<Self> {
        Self::create_with_runtime_mode(root, Some(bundled_runtime), true)
    }

    fn create_with_runtime_mode(
        root: impl Into<PathBuf>,
        bundled_runtime: Option<PathBuf>,
        defer_bundled_validation: bool,
    ) -> CoreResult<Self> {
        let root = root.into();
        let runtime_is_managed = bundled_runtime.is_none();
        let (runtime, runtime_validation) = match bundled_runtime {
            Some(runtime) => {
                let state = if defer_bundled_validation {
                    RuntimeValidationState::Pending
                } else {
                    validate_runtime_payload(&runtime)?;
                    RuntimeValidationState::Valid
                };
                (runtime, Arc::new(RuntimeValidation::new(state)))
            }
            None => {
                let runtime = root.join("runtime");
                let state = if runtime.join(RUNTIME_MANIFEST_NAME).is_file() {
                    validate_runtime_payload(&runtime)?;
                    RuntimeValidationState::Valid
                } else {
                    RuntimeValidationState::Unavailable
                };
                (runtime, Arc::new(RuntimeValidation::new(state)))
            }
        };
        let mut layout = Self {
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
            runtime_validation,
            cache: root.join("cache"),
            logs: root.join("logs"),
            temp: root.join("temp"),
            canonical_root: PathBuf::new(),
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
        if runtime_is_managed {
            fs::create_dir_all(&layout.runtime)?;
        }
        layout.canonical_root = layout.root.canonicalize()?;
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

    pub fn runtime_validated(&self) -> bool {
        matches!(
            &*self.runtime_validation.state.lock(),
            RuntimeValidationState::Valid
        )
    }

    pub(crate) fn runtime_validation_pending(&self) -> bool {
        matches!(
            &*self.runtime_validation.state.lock(),
            RuntimeValidationState::Pending | RuntimeValidationState::Validating
        )
    }

    pub(crate) fn runtime_validation_required(&self) -> bool {
        !matches!(
            &*self.runtime_validation.state.lock(),
            RuntimeValidationState::Unavailable
        )
    }

    /// Completes deferred validation once and shares the result with every
    /// runtime consumer. Callers arriving while the background validation is
    /// running wait for it; no bundled binary is returned before all payload
    /// SHA-256 checks and containment checks succeed.
    pub(crate) fn ensure_runtime_validated(&self) -> CoreResult<bool> {
        {
            let mut state = self.runtime_validation.state.lock();
            loop {
                match &*state {
                    RuntimeValidationState::Unavailable => return Ok(false),
                    RuntimeValidationState::Valid => return Ok(true),
                    RuntimeValidationState::Invalid(message) => {
                        return Err(CoreError::Security(message.clone()));
                    }
                    RuntimeValidationState::Validating => {
                        self.runtime_validation.changed.wait(&mut state);
                    }
                    RuntimeValidationState::Pending => {
                        *state = RuntimeValidationState::Validating;
                        break;
                    }
                }
            }
        }

        let _span = PerformanceSpan::new("runtime_validation", "source=bundled");
        let validation = validate_runtime_payload(&self.runtime);
        let mut state = self.runtime_validation.state.lock();
        match validation {
            Ok(()) => {
                *state = RuntimeValidationState::Valid;
                self.runtime_validation.changed.notify_all();
                Ok(true)
            }
            Err(error) => {
                let message = error.to_string();
                *state = RuntimeValidationState::Invalid(message.clone());
                self.runtime_validation.changed.notify_all();
                Err(CoreError::Security(message))
            }
        }
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
        let canonical = joined.canonicalize()?;
        if !canonical.starts_with(&self.canonical_root) {
            return Err(CoreError::Security(
                "stored asset resolves outside the application data root".into(),
            ));
        }
        Ok(canonical)
    }
}

#[allow(dead_code)]
/// Validates an immutable processing runtime before any bundled executable is
/// selected. The embedded manifest is treated as untrusted input: every file
/// must be declared exactly once, remain inside the runtime root after link
/// resolution, and match its recorded size and SHA-256 digest.
pub(crate) fn validate_runtime_payload(runtime: &Path) -> CoreResult<()> {
    let platform = current_runtime_platform()?;
    let canonical_root = runtime.canonicalize().map_err(|error| {
        runtime_validation_error(format!(
            "runtime root could not be resolved ({}): {error}",
            runtime.display()
        ))
    })?;
    let root_metadata = fs::metadata(&canonical_root)?;
    if !root_metadata.is_dir() {
        return Err(runtime_validation_error("runtime root is not a directory"));
    }

    let manifest_path = runtime.join(RUNTIME_MANIFEST_NAME);
    let manifest_link_metadata = fs::symlink_metadata(&manifest_path).map_err(|error| {
        runtime_validation_error(format!("runtime manifest is unavailable: {error}"))
    })?;
    if !manifest_link_metadata.is_file() || is_link_or_reparse(&manifest_link_metadata) {
        return Err(runtime_validation_error(
            "runtime manifest must be an ordinary file",
        ));
    }
    let canonical_manifest = manifest_path.canonicalize()?;
    if !canonical_manifest.starts_with(&canonical_root) {
        return Err(runtime_validation_error(
            "runtime manifest resolves outside the runtime root",
        ));
    }
    let manifest_size = manifest_link_metadata.len();
    if manifest_size == 0 || manifest_size > RUNTIME_MANIFEST_MAX_BYTES {
        return Err(runtime_validation_error(format!(
            "runtime manifest size must be between 1 and {RUNTIME_MANIFEST_MAX_BYTES} bytes"
        )));
    }
    let manifest: RuntimeManifest = serde_json::from_reader(File::open(&manifest_path)?)?;

    validate_runtime_identity(&manifest, platform)?;
    if manifest.payload.is_empty() {
        return Err(runtime_validation_error(
            "runtime manifest payload must not be empty",
        ));
    }

    let mut declared_paths = BTreeSet::new();
    let mut comparison_paths = HashSet::new();
    for record in &manifest.payload {
        validate_runtime_relative_path(&record.path)?;
        let comparison_path = record.path.to_ascii_lowercase();
        if !comparison_paths.insert(comparison_path) || !declared_paths.insert(record.path.clone())
        {
            return Err(runtime_validation_error(format!(
                "runtime manifest contains a duplicate payload path: {}",
                record.path
            )));
        }
        if record.sha256.len() != 64 || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(runtime_validation_error(format!(
                "runtime manifest has an invalid SHA-256 digest for {}",
                record.path
            )));
        }

        let candidate = runtime.join(Path::new(&record.path));
        let canonical_candidate = candidate.canonicalize().map_err(|error| {
            runtime_validation_error(format!(
                "runtime payload file is missing or unreadable ({}): {error}",
                record.path
            ))
        })?;
        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(runtime_validation_error(format!(
                "runtime payload path resolves outside the runtime root: {}",
                record.path
            )));
        }
        let metadata = fs::metadata(&candidate)?;
        if !metadata.is_file() {
            return Err(runtime_validation_error(format!(
                "runtime payload entry is not a file: {}",
                record.path
            )));
        }
        if metadata.len() != record.size {
            return Err(runtime_validation_error(format!(
                "runtime payload size mismatch for {}",
                record.path
            )));
        }
        let digest = sha256_file(&candidate)?;
        if !digest.eq_ignore_ascii_case(&record.sha256) {
            return Err(runtime_validation_error(format!(
                "runtime payload SHA-256 mismatch for {}",
                record.path
            )));
        }
    }

    for name in platform.executable_names {
        if !declared_paths.contains(*name) {
            return Err(runtime_validation_error(format!(
                "runtime manifest is missing required top-level entry: {name}"
            )));
        }
        let path = runtime.join(name);
        let link_metadata = fs::symlink_metadata(&path)?;
        if !link_metadata.is_file() && !link_metadata.file_type().is_symlink() {
            return Err(runtime_validation_error(format!(
                "required runtime entry is not a file: {name}"
            )));
        }
        require_executable(&path, name)?;
    }

    let enumerated_paths = enumerate_runtime_files(&canonical_root)?;
    if enumerated_paths != declared_paths {
        let missing = declared_paths.difference(&enumerated_paths).next();
        let unlisted = enumerated_paths.difference(&declared_paths).next();
        let detail = match (missing, unlisted) {
            (Some(path), _) => format!("manifest payload file is missing: {path}"),
            (_, Some(path)) => format!("runtime contains an unlisted file: {path}"),
            _ => "runtime payload enumeration does not match the manifest".into(),
        };
        return Err(runtime_validation_error(detail));
    }

    Ok(())
}

fn validate_runtime_identity(
    manifest: &RuntimeManifest,
    platform: RuntimePlatform,
) -> CoreResult<()> {
    if manifest.schema_version != RUNTIME_MANIFEST_SCHEMA_VERSION {
        return Err(runtime_validation_error(
            "runtime manifest schema version is incompatible",
        ));
    }
    if manifest.product != RUNTIME_PRODUCT {
        return Err(runtime_validation_error(
            "runtime manifest product is incompatible",
        ));
    }
    if manifest.app_identifier != APP_IDENTIFIER {
        return Err(runtime_validation_error(
            "runtime manifest application identifier is incompatible",
        ));
    }
    if manifest.runtime_version != env!("CARGO_PKG_VERSION") {
        return Err(runtime_validation_error(format!(
            "runtime version {} does not match application version {}",
            manifest.runtime_version,
            env!("CARGO_PKG_VERSION")
        )));
    }
    if manifest.architecture != platform.architecture
        || !platform.variants.contains(&manifest.variant.as_str())
    {
        return Err(runtime_validation_error(
            "runtime variant or architecture is incompatible with this application",
        ));
    }
    if manifest.worker_protocol_version != WORKER_PROTOCOL_VERSION {
        return Err(runtime_validation_error(
            "runtime worker protocol version is incompatible",
        ));
    }
    if manifest.pipeline_version != RUNTIME_PIPELINE_VERSION {
        return Err(runtime_validation_error(
            "runtime pipeline version is incompatible",
        ));
    }
    let expected_model_manifest_sha256 = hex::encode(Sha256::digest(EMBEDDED_MODEL_MANIFEST));
    if manifest.model_manifest.sha256.len() != 64
        || !manifest
            .model_manifest
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !manifest
            .model_manifest
            .sha256
            .eq_ignore_ascii_case(&expected_model_manifest_sha256)
    {
        return Err(runtime_validation_error(
            "runtime model manifest digest is incompatible",
        ));
    }
    Ok(())
}

fn validate_runtime_relative_path(relative: &str) -> CoreResult<()> {
    if relative.is_empty()
        || relative == RUNTIME_MANIFEST_NAME
        || relative.contains(['\\', '\0', ':'])
        || relative
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(runtime_validation_error(format!(
            "runtime manifest contains an unsafe payload path: {relative}"
        )));
    }
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(runtime_validation_error(format!(
            "runtime manifest contains an unsafe payload path: {relative}"
        )));
    }
    Ok(())
}

fn enumerate_runtime_files(runtime_root: &Path) -> CoreResult<BTreeSet<String>> {
    fn visit(
        runtime_root: &Path,
        directory: &Path,
        files: &mut BTreeSet<String>,
    ) -> CoreResult<()> {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() && !is_link_or_reparse(&metadata) {
                visit(runtime_root, &path, files)?;
                continue;
            }

            let relative = path
                .strip_prefix(runtime_root)
                .map_err(|_| runtime_validation_error("runtime enumeration escaped its root"))?;
            let relative = runtime_relative_string(relative)?;
            if relative == RUNTIME_MANIFEST_NAME {
                continue;
            }

            if is_link_or_reparse(&metadata) {
                let canonical = path.canonicalize().map_err(|error| {
                    runtime_validation_error(format!(
                        "runtime payload link is invalid ({relative}): {error}"
                    ))
                })?;
                if !canonical.starts_with(runtime_root) {
                    return Err(runtime_validation_error(format!(
                        "runtime payload link resolves outside the runtime root: {relative}"
                    )));
                }
                if !fs::metadata(&path)?.is_file() {
                    return Err(runtime_validation_error(format!(
                        "runtime payload links must resolve to files: {relative}"
                    )));
                }
                files.insert(relative);
            } else if metadata.is_file() {
                files.insert(relative);
            } else {
                return Err(runtime_validation_error(
                    "runtime contains an unsupported filesystem entry",
                ));
            }
        }
        Ok(())
    }

    let mut files = BTreeSet::new();
    visit(runtime_root, runtime_root, &mut files)?;
    Ok(files)
}

fn runtime_relative_string(path: &Path) -> CoreResult<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(|| {
                runtime_validation_error("runtime contains a non-UTF-8 payload path")
            })?),
            _ => {
                return Err(runtime_validation_error(
                    "runtime contains an unsafe payload path",
                ));
            }
        }
    }
    Ok(parts.join("/"))
}

fn sha256_file(path: &Path) -> CoreResult<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(unix)]
fn require_executable(path: &Path, name: &str) -> CoreResult<()> {
    use std::os::unix::fs::PermissionsExt;

    if fs::metadata(path)?.permissions().mode() & 0o111 == 0 {
        return Err(runtime_validation_error(format!(
            "required runtime entry is not executable: {name}"
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_executable(_path: &Path, _name: &str) -> CoreResult<()> {
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn current_runtime_platform() -> CoreResult<RuntimePlatform> {
    Ok(RuntimePlatform {
        architecture: "arm64",
        variants: &["apple-mlx"],
        executable_names: &["local-transcript-worker", "ffmpeg", "ffprobe"],
    })
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn current_runtime_platform() -> CoreResult<RuntimePlatform> {
    Ok(RuntimePlatform {
        architecture: "x64",
        variants: &["nvidia", "cpu"],
        executable_names: &["local-transcript-worker.exe", "ffmpeg.exe", "ffprobe.exe"],
    })
}

#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(windows, target_arch = "x86_64")
)))]
fn current_runtime_platform() -> CoreResult<RuntimePlatform> {
    Err(runtime_validation_error(
        "bundled runtimes are unsupported on this platform",
    ))
}

fn runtime_validation_error(message: impl Into<String>) -> CoreError {
    CoreError::Security(format!(
        "bundled runtime validation failed: {}",
        message.into()
    ))
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
    use serde_json::{json, Value};

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn write_valid_runtime(runtime: &Path) {
        fs::create_dir_all(runtime.join("_internal")).unwrap();
        let platform = current_runtime_platform().unwrap();
        for name in platform.executable_names {
            fs::write(runtime.join(name), format!("fixture executable {name}")).unwrap();
            mark_executable(&runtime.join(name));
        }
        fs::write(runtime.join("_internal/worker-data.bin"), b"worker data").unwrap();
        write_current_manifest(runtime);
    }

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn write_current_manifest(runtime: &Path) {
        let platform = current_runtime_platform().unwrap();
        let payload = enumerate_runtime_files(&runtime.canonicalize().unwrap())
            .unwrap()
            .into_iter()
            .map(|relative| {
                let path = runtime.join(&relative);
                json!({
                    "path": relative,
                    "size": fs::metadata(&path).unwrap().len(),
                    "sha256": sha256_file(&path).unwrap(),
                })
            })
            .collect::<Vec<_>>();
        let manifest = json!({
            "schema_version": RUNTIME_MANIFEST_SCHEMA_VERSION,
            "product": RUNTIME_PRODUCT,
            "app_identifier": APP_IDENTIFIER,
            "runtime_version": env!("CARGO_PKG_VERSION"),
            "variant": platform.variants[0],
            "architecture": platform.architecture,
            "worker_protocol_version": WORKER_PROTOCOL_VERSION,
            "pipeline_version": RUNTIME_PIPELINE_VERSION,
            "model_manifest": {
                "sha256": hex::encode(Sha256::digest(EMBEDDED_MODEL_MANIFEST)),
            },
            "payload": payload,
        });
        write_manifest(runtime, &manifest);
    }

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn read_manifest(runtime: &Path) -> Value {
        serde_json::from_slice(&fs::read(runtime.join(RUNTIME_MANIFEST_NAME)).unwrap()).unwrap()
    }

    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn write_manifest(runtime: &Path, manifest: &Value) {
        fs::write(
            runtime.join(RUNTIME_MANIFEST_NAME),
            serde_json::to_vec_pretty(manifest).unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn mark_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn mark_executable(_path: &Path) {}

    #[test]
    fn rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let layout = AppLayout::create(temp.path()).unwrap();
        assert!(layout.resolve_relative("../secret").is_err());
        assert!(layout.resolve_relative("C:/Windows/notepad.exe").is_err());
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn uses_complete_bundled_runtime_without_copying_it_into_user_data() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("installed-resources").join("runtime");
        write_valid_runtime(&bundled);

        let layout =
            AppLayout::create_with_runtime(temp.path().join("app-data"), Some(bundled.clone()))
                .unwrap();

        assert_eq!(layout.runtime(), bundled);
        assert!(layout.runtime_validated());
        assert!(!layout.root().join("runtime").exists());
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn deferred_runtime_is_fully_validated_before_becoming_executable() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("installed-resources").join("runtime");
        write_valid_runtime(&bundled);

        let layout =
            AppLayout::create_with_deferred_runtime(temp.path().join("app-data"), bundled.clone())
                .unwrap();

        assert!(!layout.runtime_validated());
        assert!(layout.runtime_validation_required());
        assert!(layout.runtime_validation_pending());
        assert!(layout.ensure_runtime_validated().unwrap());
        assert!(layout.runtime_validated());
        assert!(!layout.runtime_validation_pending());
        assert_eq!(layout.runtime(), bundled);
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn deferred_runtime_never_accepts_a_tampered_payload() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("installed-resources").join("runtime");
        write_valid_runtime(&bundled);
        let layout =
            AppLayout::create_with_deferred_runtime(temp.path().join("app-data"), bundled.clone())
                .unwrap();
        fs::write(bundled.join("_internal/worker-data.bin"), b"tampered!!!").unwrap();

        assert!(layout.ensure_runtime_validated().is_err());
        assert!(!layout.runtime_validated());

        // A failed validation is sticky for this process. Restoring bytes does
        // not race another caller into executing a payload whose identity
        // changed underneath the original validation attempt.
        fs::write(bundled.join("_internal/worker-data.bin"), b"worker data").unwrap();
        assert!(layout.ensure_runtime_validated().is_err());
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn validates_an_installer_managed_runtime_inside_app_data() {
        let temp = tempfile::tempdir().unwrap();
        let app_data = temp.path().join("app-data");
        write_valid_runtime(&app_data.join("runtime"));

        let layout = AppLayout::create(&app_data).unwrap();

        assert!(layout.runtime_validated());
        assert_eq!(layout.runtime(), app_data.join("runtime"));
    }

    #[test]
    fn rejects_incomplete_bundled_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("runtime");
        fs::create_dir_all(&bundled).unwrap();
        let ffmpeg = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        fs::write(bundled.join(ffmpeg), b"runtime").unwrap();

        let result = AppLayout::create_with_runtime(temp.path().join("app-data"), Some(bundled));

        assert!(result.is_err());
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn runtime_manifest_rejects_incompatible_identity_and_versions() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        write_valid_runtime(&runtime);
        let valid = read_manifest(&runtime);

        for (field, bad_value) in [
            ("schema_version", json!(2)),
            ("product", json!("Another Runtime")),
            ("app_identifier", json!("example.invalid")),
            ("runtime_version", json!("999.0.0")),
            ("variant", json!("incompatible")),
            ("architecture", json!("incompatible")),
            ("worker_protocol_version", json!("2.0")),
            ("pipeline_version", json!("incompatible")),
        ] {
            let mut changed = valid.clone();
            changed[field] = bad_value;
            write_manifest(&runtime, &changed);
            assert!(
                validate_runtime_payload(&runtime).is_err(),
                "accepted incompatible {field}"
            );
        }

        let mut changed = valid;
        changed["model_manifest"]["sha256"] = json!("0".repeat(64));
        write_manifest(&runtime, &changed);
        assert!(
            validate_runtime_payload(&runtime).is_err(),
            "accepted incompatible embedded model manifest"
        );
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn runtime_manifest_rejects_tampering_and_missing_internal_files() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        write_valid_runtime(&runtime);
        let internal = runtime.join("_internal/worker-data.bin");

        fs::write(&internal, b"tampered!!!").unwrap();
        assert!(validate_runtime_payload(&runtime).is_err());

        fs::write(&internal, b"worker data").unwrap();
        assert!(validate_runtime_payload(&runtime).is_ok());
        fs::remove_file(&internal).unwrap();
        assert!(validate_runtime_payload(&runtime).is_err());
    }

    #[test]
    #[cfg(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(windows, target_arch = "x86_64")
    ))]
    fn runtime_manifest_rejects_unsafe_duplicate_and_unlisted_paths() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        write_valid_runtime(&runtime);
        let valid = read_manifest(&runtime);

        let mut unsafe_manifest = valid.clone();
        let mut unsafe_record = unsafe_manifest["payload"][0].clone();
        unsafe_record["path"] = json!("../outside");
        unsafe_manifest["payload"]
            .as_array_mut()
            .unwrap()
            .push(unsafe_record);
        write_manifest(&runtime, &unsafe_manifest);
        assert!(validate_runtime_payload(&runtime).is_err());

        let mut duplicate_manifest = valid.clone();
        let duplicate = duplicate_manifest["payload"][0].clone();
        duplicate_manifest["payload"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        write_manifest(&runtime, &duplicate_manifest);
        assert!(validate_runtime_payload(&runtime).is_err());

        write_manifest(&runtime, &valid);
        fs::write(runtime.join("_internal/unlisted.bin"), b"unlisted").unwrap();
        assert!(validate_runtime_payload(&runtime).is_err());
    }

    #[test]
    #[cfg(all(
        unix,
        any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(windows, target_arch = "x86_64")
        )
    ))]
    fn runtime_manifest_accepts_in_root_file_links_and_rejects_escaping_links() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        write_valid_runtime(&runtime);
        let target = runtime.join("_internal/worker-data.bin");
        let link = runtime.join("_internal/worker-data-link.bin");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_current_manifest(&runtime);
        assert!(validate_runtime_payload(&runtime).is_ok());

        let outside = temp.path().join("outside.bin");
        fs::write(&outside, b"outside").unwrap();
        std::os::unix::fs::symlink(&outside, runtime.join("_internal/escape.bin")).unwrap();
        assert!(validate_runtime_payload(&runtime).is_err());
    }

    #[test]
    #[cfg(all(
        unix,
        any(
            all(target_os = "macos", target_arch = "aarch64"),
            all(windows, target_arch = "x86_64")
        )
    ))]
    fn runtime_manifest_requires_executable_entrypoints() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        write_valid_runtime(&runtime);
        let worker = runtime.join(current_runtime_platform().unwrap().executable_names[0]);
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(validate_runtime_payload(&runtime).is_err());
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
