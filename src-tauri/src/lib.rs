mod asset_protocol;
mod audio;
mod commands;
mod crypto;
mod db;
mod error;
mod job_coordinator;
mod layout;
mod local_agent;
mod media;
mod media_tools;
mod models;
mod recording;
mod screen_capture;
mod service;
mod visual_context;
mod worker;

use std::{
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::Arc,
};

use error::{CoreError, CoreResult};
use job_coordinator::JobCoordinator;
use recording::RecordingManager;
use service::CoreService;
use tauri::Manager;
use worker::WorkerSupervisor;

pub struct AppState {
    pub core: Arc<CoreService>,
    pub recording: Arc<RecordingManager>,
    pub worker: Arc<WorkerSupervisor>,
    pub jobs: Arc<JobCoordinator>,
}

const DEVELOPMENT_RUNTIME_ENV: &str = "SAYTRACE_DEV_RUNTIME";

fn select_processing_runtime(
    bundled_runtime: &Path,
    development_override: Option<&OsStr>,
) -> CoreResult<Option<PathBuf>> {
    if layout::runtime_payload_ready(bundled_runtime) {
        return Ok(Some(bundled_runtime.to_path_buf()));
    }
    let Some(raw_override) = development_override.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let override_path = PathBuf::from(raw_override);
    if !layout::runtime_payload_ready(&override_path) {
        return Err(CoreError::InvalidInput(format!(
            "{DEVELOPMENT_RUNTIME_ENV} does not point to a complete SayTrace processing runtime: {}",
            override_path.display()
        )));
    }
    Ok(Some(override_path))
}

fn configured_processing_runtime(bundled_runtime: &Path) -> CoreResult<Option<PathBuf>> {
    #[cfg(debug_assertions)]
    let development_override: Option<OsString> = env::var_os(DEVELOPMENT_RUNTIME_ENV);
    #[cfg(not(debug_assertions))]
    let development_override: Option<OsString> = None;

    select_processing_runtime(bundled_runtime, development_override.as_deref())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Media and models can be very large and must not follow a user
            // through a roaming Windows profile. The processing runtime is an
            // immutable app resource owned by the normal installer.
            let app_data = app.path().app_local_data_dir()?;
            let bundled_runtime = app.path().resource_dir()?.join("runtime");
            let core = match configured_processing_runtime(&bundled_runtime)? {
                Some(runtime) => Arc::new(CoreService::open_with_runtime(app_data, runtime)?),
                None => {
                    // Development builds may use the managed runtime directory or
                    // the explicitly permitted development worker/tool fallback.
                    Arc::new(CoreService::open(app_data)?)
                }
            };
            core.recover_interrupted_work()?;
            let worker = Arc::new(WorkerSupervisor::new(core.layout().clone()));
            let recording = Arc::new(RecordingManager::new(core.clone(), worker.clone()));
            let jobs = Arc::new(JobCoordinator::start(
                core.clone(),
                worker.clone(),
                app.handle().clone(),
            )?);
            app.manage(AppState {
                core,
                recording,
                worker,
                jobs,
            });
            Ok(())
        })
        .register_uri_scheme_protocol("localtranscript", |context, request| {
            let state = context.app_handle().state::<AppState>();
            asset_protocol::handle(&state.core, request)
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_app_status,
            commands::get_library_stats,
            commands::import_media,
            commands::list_meetings,
            commands::get_meeting,
            commands::rename_meeting,
            commands::delete_meeting,
            commands::search_transcript,
            commands::get_local_agent_status,
            commands::list_transcript_chat,
            commands::ask_transcript,
            commands::clear_transcript_chat,
            commands::update_transcript_turn,
            commands::set_transcript_turn_review,
            commands::set_transcript_turn_bookmark,
            commands::rename_speaker,
            commands::merge_speakers,
            commands::set_speaker_review,
            commands::review_speaker,
            commands::list_voice_profiles,
            commands::create_voice_profile,
            commands::delete_voice_profile,
            commands::confirm_voice_profile_sample,
            commands::list_processing_jobs,
            commands::cancel_processing_job,
            commands::retry_processing_job,
            commands::export_transcript,
            commands::backup_library,
            commands::create_backup,
            commands::get_asset_descriptor,
            commands::read_asset_chunk,
            commands::list_audio_devices,
            commands::get_recording_status,
            commands::start_recording,
            commands::pause_recording,
            commands::resume_recording,
            commands::add_recording_marker,
            commands::add_marker,
            commands::stop_recording,
            commands::get_worker_status,
            commands::restart_worker,
            commands::get_model_status,
            commands::install_model_pack,
        ])
        .run(tauri::generate_context!())
        .expect("error while running SayTrace");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn complete_runtime(path: &Path) {
        fs::create_dir_all(path).unwrap();
        for name in [
            "local-transcript-worker.exe",
            "ffmpeg.exe",
            "ffprobe.exe",
            "runtime-manifest.json",
        ] {
            fs::write(path.join(name), b"runtime").unwrap();
        }
    }

    #[test]
    fn bundled_runtime_takes_priority_over_development_override() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("bundled");
        let development = temp.path().join("development");
        complete_runtime(&bundled);
        complete_runtime(&development);

        assert_eq!(
            select_processing_runtime(&bundled, Some(development.as_os_str())).unwrap(),
            Some(bundled)
        );
    }

    #[test]
    fn complete_development_runtime_is_used_when_bundle_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        let bundled = temp.path().join("missing-bundle");
        let development = temp.path().join("development");
        complete_runtime(&development);

        assert_eq!(
            select_processing_runtime(&bundled, Some(development.as_os_str())).unwrap(),
            Some(development)
        );
    }

    #[test]
    fn incomplete_development_runtime_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let error = select_processing_runtime(
            &temp.path().join("missing-bundle"),
            Some(temp.path().join("incomplete").as_os_str()),
        )
        .unwrap_err();

        assert!(matches!(error, CoreError::InvalidInput(_)));
        assert!(error.to_string().contains(DEVELOPMENT_RUNTIME_ENV));
    }
}
