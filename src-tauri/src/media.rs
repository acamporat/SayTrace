use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    error::{CoreError, CoreResult},
    layout::AppLayout,
    media_tools::{self, MediaTool},
};

const COPY_BUFFER_BYTES: usize = 1024 * 1024;
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "avi", "flac", "m4a", "m4v", "mkv", "mov", "mp3", "mp4", "mpeg", "mpg",
    "oga", "ogg", "opus", "wav", "webm", "wma", "wmv",
];

#[derive(Debug, Clone)]
pub struct MediaProbe {
    pub kind: String,
    pub content_type: String,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    #[serde(default)]
    streams: Vec<FfprobeStream>,
    format: Option<FfprobeFormat>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

pub fn validate_source(path: &Path) -> CoreResult<()> {
    if !path.is_absolute() {
        return Err(CoreError::InvalidInput(
            "media source must be an absolute path selected by the user".into(),
        ));
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(CoreError::InvalidInput(
            "selected media source is not a file".into(),
        ));
    }
    let extension = extension_lower(path);
    if !SUPPORTED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(CoreError::Media(format!(
            "unsupported media extension .{extension}"
        )));
    }
    Ok(())
}

pub fn probe(layout: &AppLayout, path: &Path) -> CoreResult<MediaProbe> {
    validate_source(path)?;
    ffprobe(&media_tools::resolve(layout, MediaTool::Ffprobe)?, path)
}

fn ffprobe(executable: &Path, path: &Path) -> CoreResult<MediaProbe> {
    let mut command = Command::new(executable);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration:stream=codec_type,codec_name,sample_rate,channels",
        "-of",
        "json",
    ]);
    command.arg(path);
    command.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CoreError::MediaToolMissing(
                "FFprobe is required to validate imported audio and video".into(),
            )
        } else {
            CoreError::Media(format!("ffprobe could not start: {error}"))
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CoreError::Media(format!(
            "ffprobe rejected the selected media: {}",
            stderr.trim()
        )));
    }
    let parsed: FfprobeOutput = serde_json::from_slice(&output.stdout)?;
    let audio = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));
    if audio.is_none() {
        return Err(CoreError::Media(
            "selected file does not contain an audio stream".into(),
        ));
    }
    let has_video = parsed
        .streams
        .iter()
        .any(|stream| stream.codec_type.as_deref() == Some("video"));
    let duration_ms = parsed
        .format
        .and_then(|format| format.duration)
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .map(|duration| (duration * 1000.0).round() as i64);
    Ok(MediaProbe {
        kind: if has_video { "video" } else { "audio" }.into(),
        content_type: mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .into(),
        duration_ms,
        codec: audio.and_then(|stream| stream.codec_name.clone()),
        sample_rate_hz: audio
            .and_then(|stream| stream.sample_rate.as_ref())
            .and_then(|rate| rate.parse::<u32>().ok()),
        channels: audio.and_then(|stream| stream.channels),
    })
}

pub fn atomic_copy_with_hash(source: &Path, destination: &Path) -> CoreResult<(u64, String)> {
    let parent = destination
        .parent()
        .ok_or_else(|| CoreError::InvalidInput("destination has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let partial = partial_path(destination);
    let result = (|| -> CoreResult<(u64, String)> {
        let input = File::open(source)?;
        let output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        let mut reader = BufReader::with_capacity(COPY_BUFFER_BYTES, input);
        let mut writer = BufWriter::with_capacity(COPY_BUFFER_BYTES, output);
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            writer.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            total = total.saturating_add(read as u64);
        }
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(&partial, destination)?;
        sync_directory(parent);
        Ok((total, hex::encode(hasher.finalize())))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

pub fn atomic_write(destination: &Path, bytes: &[u8]) -> CoreResult<(u64, String)> {
    let parent = destination
        .parent()
        .ok_or_else(|| CoreError::InvalidInput("destination has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let partial = partial_path(destination);
    let result = (|| -> CoreResult<(u64, String)> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&partial, destination)?;
        sync_directory(parent);
        Ok((bytes.len() as u64, sha256_bytes(bytes)))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

pub fn sha256_file(path: &Path) -> CoreResult<String> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sanitize_file_stem(value: &str) -> String {
    let mut result = String::with_capacity(value.len().min(80));
    for character in value.chars().take(80) {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ' ') {
            result.push(character);
        } else {
            result.push('_');
        }
    }
    let trimmed = result.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "transcript".into()
    } else {
        trimmed.into()
    }
}

pub fn extension_lower(path: &Path) -> String {
    path.extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub fn partial_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("file");
    destination.with_file_name(format!("{name}.partial"))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_copy_keeps_bytes_and_hash() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.wav");
        let contents = b"not really wav, but exact bytes";
        fs::write(&source, contents).unwrap();
        let destination = temp.path().join("managed").join("copy.wav");
        let (size, hash) = atomic_copy_with_hash(&source, &destination).unwrap();
        assert_eq!(size, contents.len() as u64);
        assert_eq!(fs::read(destination).unwrap(), fs::read(source).unwrap());
        assert_eq!(hash, sha256_bytes(contents));
        assert!(!temp
            .path()
            .join("managed")
            .join("copy.wav.partial")
            .exists());
    }

    #[test]
    fn fallback_rejects_non_media_extensions() {
        let temp = tempfile::tempdir().unwrap();
        let layout = AppLayout::create(temp.path().join("app")).unwrap();
        let source = temp.path().join("notes.txt");
        fs::write(&source, b"text").unwrap();
        assert!(probe(&layout, &source).is_err());
    }
}
