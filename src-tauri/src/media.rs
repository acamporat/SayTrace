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
#[cfg(windows)]
const WEBVIEW_H264_ENCODER_ARGS: &[&str] = &[
    "-c:v",
    "h264_mf",
    "-rate_control",
    "quality",
    "-quality",
    "75",
    "-scenario",
    "archive",
    "-g",
    "10",
    "-pix_fmt",
    "yuv420p",
    "-movflags",
    "+faststart",
    "-f",
    "mp4",
];
#[cfg(target_os = "macos")]
const WEBVIEW_H264_ENCODER_ARGS: &[&str] = &[
    "-c:v",
    "h264_videotoolbox",
    "-q:v",
    "65",
    "-profile:v",
    "high",
    "-g",
    "10",
    "-pix_fmt",
    "yuv420p",
    "-movflags",
    "+faststart",
    "-f",
    "mp4",
];
#[cfg(not(any(windows, target_os = "macos")))]
const WEBVIEW_H264_ENCODER_ARGS: &[&str] = &[];
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
    index: Option<u32>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
    #[serde(default)]
    disposition: FfprobeDisposition,
}

#[derive(Debug, Default, Deserialize)]
struct FfprobeDisposition {
    #[serde(default)]
    attached_pic: i32,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
}

#[derive(Debug)]
struct ProbeResult {
    media: MediaProbe,
    video_stream_index: Option<u32>,
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
    Ok(ffprobe(
        &media_tools::resolve(layout, MediaTool::Ffprobe)?,
        path,
        RequiredStream::Audio,
    )?
    .media)
}

/// Probe a screen recording that is intentionally video-only. Imported meeting
/// media still goes through [`probe`] and must contain an audio stream.
pub fn probe_visual(layout: &AppLayout, path: &Path) -> CoreResult<MediaProbe> {
    validate_source(path)?;
    Ok(ffprobe(
        &media_tools::resolve(layout, MediaTool::Ffprobe)?,
        path,
        RequiredStream::Video,
    )?
    .media)
}

/// Probe an installer-managed visual file before it is published under its
/// final extension. Recording recovery uses this for fragmented
/// `*.mp4.partial` capture files and for atomic encoded outputs. The caller must
/// already own the path; unlike imported media, no extension allowlist is
/// applied.
pub(crate) fn probe_unpublished_visual(layout: &AppLayout, path: &Path) -> CoreResult<MediaProbe> {
    if !path.is_absolute() {
        return Err(CoreError::InvalidInput(
            "unpublished visual source must be an absolute managed path".into(),
        ));
    }
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(CoreError::InvalidInput(
            "unpublished visual source is not a file".into(),
        ));
    }
    layout.relative_to_root(path)?;
    Ok(ffprobe(
        &media_tools::resolve(layout, MediaTool::Ffprobe)?,
        path,
        RequiredStream::Video,
    )?
    .media)
}

/// Return the global FFmpeg stream index for the first real visual stream.
/// Attached cover art is deliberately excluded so audio imports cannot be
/// mistaken for meeting video context.
pub(crate) fn probe_visual_stream_index(layout: &AppLayout, path: &Path) -> CoreResult<u32> {
    validate_source(path)?;
    ffprobe(
        &media_tools::resolve(layout, MediaTool::Ffprobe)?,
        path,
        RequiredStream::Video,
    )?
    .video_stream_index
    .ok_or_else(|| CoreError::Media("visual stream has no FFmpeg stream index".into()))
}

/// Encode a video-only MP4 using the platform-native H.264 encoder.
///
/// Windows uses Media Foundation and macOS uses VideoToolbox. H.264 in an MP4
/// container with 4:2:0 video is supported by both platform webviews. The
/// destination is published only after FFmpeg succeeds and FFprobe confirms the
/// expected codec, so callers never persist a partially encoded derivative.
pub fn transcode_visual_for_webview(
    layout: &AppLayout,
    source: &Path,
    destination: &Path,
) -> CoreResult<MediaProbe> {
    if !cfg!(any(windows, target_os = "macos")) {
        return Err(CoreError::Media(
            "WebView-compatible screen encoding requires Media Foundation on Windows or VideoToolbox on macOS".into(),
        ));
    }
    validate_source(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| CoreError::InvalidInput("video destination has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let partial = mp4_partial_path(destination);
    let _ = fs::remove_file(&partial);

    let result = (|| -> CoreResult<MediaProbe> {
        let executable = media_tools::resolve(layout, MediaTool::Ffmpeg)?;
        let mut command = Command::new(executable);
        command.args(["-y", "-hide_banner", "-v", "error", "-i"]);
        command.arg(source);
        command.args(["-an", "-vf", "format=yuv420p", "-fps_mode", "cfr"]);
        command.args(webview_h264_encoder_args());
        command.arg(&partial);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().map_err(|error| {
            CoreError::Media(format!(
                "FFmpeg {} H.264 encoding could not start: {error}",
                webview_h264_backend_name()
            ))
        })?;
        if !output.status.success() {
            return Err(CoreError::Media(format!(
                "FFmpeg {} H.264 encoding failed: {}",
                webview_h264_backend_name(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let probe = probe_visual(layout, &partial)?;
        require_webview_h264(&probe)?;
        let _ = fs::remove_file(destination);
        fs::rename(&partial, destination)?;
        sync_directory(parent);
        Ok(probe)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

pub(crate) fn webview_h264_encoder_args() -> &'static [&'static str] {
    WEBVIEW_H264_ENCODER_ARGS
}

fn webview_h264_backend_name() -> &'static str {
    if cfg!(windows) {
        "Media Foundation"
    } else if cfg!(target_os = "macos") {
        "VideoToolbox"
    } else {
        "platform-native"
    }
}

pub(crate) fn require_webview_h264(probe: &MediaProbe) -> CoreResult<()> {
    if probe.codec.as_deref() == Some("h264") {
        Ok(())
    } else {
        Err(CoreError::Media(format!(
            "screen playback encoding produced {}, expected H.264",
            probe.codec.as_deref().unwrap_or("an unknown codec")
        )))
    }
}

#[derive(Clone, Copy)]
enum RequiredStream {
    Audio,
    Video,
}

fn ffprobe(executable: &Path, path: &Path, required: RequiredStream) -> CoreResult<ProbeResult> {
    let mut command = Command::new(executable);
    command.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration:stream=index,codec_type,codec_name,sample_rate,channels:stream_disposition=attached_pic",
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
    probe_from_output(parsed, path, required)
}

fn probe_from_output(
    parsed: FfprobeOutput,
    path: &Path,
    required: RequiredStream,
) -> CoreResult<ProbeResult> {
    let audio = parsed
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"));
    let video = parsed.streams.iter().find(|stream| {
        stream.codec_type.as_deref() == Some("video") && stream.disposition.attached_pic == 0
    });
    match required {
        RequiredStream::Audio if audio.is_none() => {
            return Err(CoreError::Media(
                "selected file does not contain an audio stream".into(),
            ));
        }
        RequiredStream::Video if video.is_none() => {
            return Err(CoreError::Media(
                "selected media does not contain a timed visual stream".into(),
            ));
        }
        _ => {}
    }
    let duration_ms = parsed
        .format
        .as_ref()
        .and_then(|format| format.duration.as_ref())
        .and_then(|duration| duration.parse::<f64>().ok())
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .map(|duration| (duration * 1000.0).round() as i64);
    Ok(ProbeResult {
        media: MediaProbe {
            kind: if video.is_some() { "video" } else { "audio" }.into(),
            content_type: mime_guess::from_path(path)
                .first_or_octet_stream()
                .essence_str()
                .into(),
            duration_ms,
            codec: match required {
                RequiredStream::Audio => audio.and_then(|stream| stream.codec_name.clone()),
                RequiredStream::Video => video.and_then(|stream| stream.codec_name.clone()),
            },
            sample_rate_hz: audio
                .and_then(|stream| stream.sample_rate.as_ref())
                .and_then(|rate| rate.parse::<u32>().ok()),
            channels: audio.and_then(|stream| stream.channels),
        },
        video_stream_index: video.and_then(|stream| stream.index),
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

fn mp4_partial_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("screen.mp4");
    destination.with_file_name(format!("{name}.partial.mp4"))
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

    #[cfg(windows)]
    #[test]
    fn webview_video_args_use_media_foundation_h264_without_gpl_codecs() {
        let arguments = webview_h264_encoder_args();
        assert!(arguments.windows(2).any(|pair| pair == ["-c:v", "h264_mf"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-pix_fmt", "yuv420p"]));
        assert!(arguments.contains(&"+faststart"));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("libx264")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn webview_video_args_use_videotoolbox_h264_without_gpl_codecs() {
        let arguments = webview_h264_encoder_args();
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-c:v", "h264_videotoolbox"]));
        assert!(arguments.windows(2).any(|pair| pair == ["-q:v", "65"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-profile:v", "high"]));
        assert!(arguments
            .windows(2)
            .any(|pair| pair == ["-pix_fmt", "yuv420p"]));
        assert!(arguments.contains(&"+faststart"));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("libx264")));
    }

    #[test]
    fn webview_video_validation_rejects_mpeg4_part_two() {
        let mut probe = MediaProbe {
            kind: "video".into(),
            content_type: "video/mp4".into(),
            duration_ms: Some(1_000),
            codec: Some("mpeg4".into()),
            sample_rate_hz: None,
            channels: None,
        };
        assert!(require_webview_h264(&probe).is_err());
        probe.codec = Some("h264".into());
        assert!(require_webview_h264(&probe).is_ok());
    }

    #[test]
    fn attached_cover_art_does_not_make_an_audio_import_visual() {
        let parsed: FfprobeOutput = serde_json::from_value(serde_json::json!({
            "streams": [
                {
                    "index": 0,
                    "codec_type": "audio",
                    "codec_name": "aac",
                    "sample_rate": "48000",
                    "channels": 2
                },
                {
                    "index": 1,
                    "codec_type": "video",
                    "codec_name": "mjpeg",
                    "disposition": { "attached_pic": 1 }
                }
            ],
            "format": { "duration": "12.5" }
        }))
        .unwrap();

        let result =
            probe_from_output(parsed, Path::new("meeting.m4a"), RequiredStream::Audio).unwrap();

        assert_eq!(result.media.kind, "audio");
        assert_eq!(result.media.codec.as_deref(), Some("aac"));
        assert_eq!(result.video_stream_index, None);
    }

    #[test]
    fn real_video_wins_over_attached_art_and_retains_global_stream_index() {
        let parsed: FfprobeOutput = serde_json::from_value(serde_json::json!({
            "streams": [
                {
                    "index": 0,
                    "codec_type": "audio",
                    "codec_name": "opus",
                    "sample_rate": "48000",
                    "channels": 2
                },
                {
                    "index": 1,
                    "codec_type": "video",
                    "codec_name": "mjpeg",
                    "disposition": { "attached_pic": 1 }
                },
                {
                    "index": 4,
                    "codec_type": "video",
                    "codec_name": "h264",
                    "disposition": { "attached_pic": 0 }
                }
            ],
            "format": { "duration": "42.0" }
        }))
        .unwrap();

        let result =
            probe_from_output(parsed, Path::new("meeting.mkv"), RequiredStream::Video).unwrap();

        assert_eq!(result.media.kind, "video");
        assert_eq!(result.media.codec.as_deref(), Some("h264"));
        assert_eq!(result.video_stream_index, Some(4));
    }
}
