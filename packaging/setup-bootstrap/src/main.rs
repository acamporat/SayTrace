#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK,
};

const TRAILER_MAGIC: &[u8; 16] = b"LTRSFXBUNDLE0001";
const TRAILER_SIZE: u64 = 112;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug)]
struct BundleTrailer {
    extractor_offset: u64,
    extractor_length: u64,
    archive_offset: u64,
    archive_length: u64,
    extractor_sha256: [u8; 32],
    archive_sha256: [u8; 32],
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn message(text: &str, error: bool) {
    let text = wide(OsStr::new(text));
    let title = wide(OsStr::new("SayTrace Setup"));
    let icon = if error {
        MB_ICONERROR
    } else {
        MB_ICONINFORMATION
    };
    // SAFETY: both strings are valid, NUL-terminated UTF-16 buffers for the
    // duration of this synchronous Windows API call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | icon,
        );
    }
}

fn read_u64(input: &[u8], offset: usize) -> io::Result<u64> {
    let bytes: [u8; 8] = input
        .get(offset..offset + 8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "truncated setup trailer"))?
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid setup trailer"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_trailer(bundle: &mut File) -> io::Result<BundleTrailer> {
    let length = bundle.metadata()?.len();
    if length < TRAILER_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "setup bundle has no payload trailer",
        ));
    }
    bundle.seek(SeekFrom::Start(length - TRAILER_SIZE))?;
    let mut trailer = [0_u8; TRAILER_SIZE as usize];
    bundle.read_exact(&mut trailer)?;
    if &trailer[..TRAILER_MAGIC.len()] != TRAILER_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "setup payload marker is missing",
        ));
    }
    let extractor_offset = read_u64(&trailer, 16)?;
    let extractor_length = read_u64(&trailer, 24)?;
    let archive_offset = read_u64(&trailer, 32)?;
    let archive_length = read_u64(&trailer, 40)?;
    let extractor_sha256 = trailer[48..80]
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid extractor hash"))?;
    let archive_sha256 = trailer[80..112]
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid archive hash"))?;
    let data_end = length - TRAILER_SIZE;
    if extractor_offset
        .checked_add(extractor_length)
        .filter(|end| *end == archive_offset)
        .and_then(|_| archive_offset.checked_add(archive_length))
        != Some(data_end)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "setup payload offsets are inconsistent",
        ));
    }
    Ok(BundleTrailer {
        extractor_offset,
        extractor_length,
        archive_offset,
        archive_length,
        extractor_sha256,
        archive_sha256,
    })
}

fn copy_verified_segment(
    bundle: &mut File,
    offset: u64,
    length: u64,
    expected_hash: &[u8; 32],
    destination: &Path,
) -> io::Result<()> {
    bundle.seek(SeekFrom::Start(offset))?;
    let mut output = File::create(destination)?;
    let mut remaining = length;
    let mut buffer = vec![0_u8; 4 * 1024 * 1024];
    let mut hasher = Sha256::new();
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| io::Error::other("setup payload length overflow"))?;
        bundle.read_exact(&mut buffer[..requested])?;
        output.write_all(&buffer[..requested])?;
        hasher.update(&buffer[..requested]);
        remaining -= requested as u64;
    }
    output.sync_all()?;
    let actual: [u8; 32] = hasher.finalize().into();
    if &actual != expected_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "setup payload failed its SHA-256 check",
        ));
    }
    Ok(())
}

fn create_private_temp() -> io::Result<PathBuf> {
    let base = env::temp_dir();
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0_u32..32 {
        let candidate = base.join(format!(
            "LocalTranscriptSetup-{}-{epoch}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "unable to allocate a private setup directory",
    ))
}

fn run(inner_arguments: &[std::ffi::OsString]) -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| format!("Cannot locate setup: {error}"))?;
    let mut bundle =
        File::open(&executable).map_err(|error| format!("Cannot open setup: {error}"))?;
    let trailer =
        read_trailer(&mut bundle).map_err(|error| format!("Invalid setup bundle: {error}"))?;
    let temporary =
        create_private_temp().map_err(|error| format!("Cannot create setup workspace: {error}"))?;
    let result = (|| {
        let extractor = temporary.join("7za.exe");
        let archive = temporary.join("payload.7z");
        copy_verified_segment(
            &mut bundle,
            trailer.extractor_offset,
            trailer.extractor_length,
            &trailer.extractor_sha256,
            &extractor,
        )
        .map_err(|error| format!("Cannot verify setup extractor: {error}"))?;
        copy_verified_segment(
            &mut bundle,
            trailer.archive_offset,
            trailer.archive_length,
            &trailer.archive_sha256,
            &archive,
        )
        .map_err(|error| format!("Cannot verify setup payload: {error}"))?;

        let extraction = Command::new(&extractor)
            .arg("x")
            .arg(&archive)
            .arg(format!("-o{}", temporary.display()))
            .args(["-y", "-bso0", "-bsp0", "-bse1"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| format!("Cannot unpack setup: {error}"))?;
        if !extraction.status.success() {
            return Err(format!(
                "Setup unpacking failed: {}",
                String::from_utf8_lossy(&extraction.stderr).trim()
            ));
        }
        let inner_setup = temporary.join("Local-Transcript-App-Installer.exe");
        if !inner_setup.is_file()
            || !temporary
                .join("runtime")
                .join("runtime-manifest.json")
                .is_file()
            || !temporary.join("install-runtime.ps1").is_file()
        {
            return Err("Setup payload is incomplete.".to_string());
        }
        let status = Command::new(&inner_setup)
            .args(inner_arguments)
            .status()
            .map_err(|error| format!("Cannot start SayTrace installer: {error}"))?;
        if !status.success() {
            return Err(format!(
                "SayTrace installation did not complete (exit code {}).",
                status.code().unwrap_or(-1)
            ));
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&temporary);
    result
}

fn main() {
    let inner_arguments: Vec<_> = env::args_os().skip(1).collect();
    let silent = inner_arguments
        .iter()
        .any(|argument| argument.eq_ignore_ascii_case("/S"));
    if !silent {
        message(
            "SayTrace Setup will unpack the included offline processing runtime, then open the normal installer. This can take a minute.",
            false,
        );
    }
    if let Err(error) = run(&inner_arguments) {
        if !silent {
            message(&error, true);
        }
        std::process::exit(1);
    }
}
