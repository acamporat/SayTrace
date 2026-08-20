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
mod performance;
mod recording;
mod screen_capture;
mod service;
mod visual_context;
mod worker;

use std::{sync::Arc, thread};

use job_coordinator::JobCoordinator;
use recording::RecordingManager;
use serde_json::json;
use service::CoreService;
use tauri::{Emitter, Manager};
use worker::WorkerSupervisor;

pub struct AppState {
    pub core: Arc<CoreService>,
    pub recording: Arc<RecordingManager>,
    pub worker: Arc<WorkerSupervisor>,
    pub jobs: Arc<JobCoordinator>,
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Media and models can be very large, so keep them in the local app
            // data container. The processing runtime is an immutable app
            // resource owned by the normal installer or app bundle.
            let app_data = app.path().app_local_data_dir()?;
            let bundled_runtime = app.path().resource_dir()?.join("runtime");
            let core = if bundled_runtime.join("runtime-manifest.json").is_file() {
                Arc::new(CoreService::open_with_deferred_runtime(
                    app_data,
                    bundled_runtime,
                )?)
            } else {
                // Development builds may use the managed runtime directory or
                // the explicitly permitted development worker/tool fallback.
                Arc::new(CoreService::open(app_data)?)
            };
            let recovery_scope = core.capture_startup_recovery_scope()?;
            core.recover_interrupted_processing_jobs()?;
            let worker = Arc::new(WorkerSupervisor::new(core.layout().clone()));
            let recording = Arc::new(RecordingManager::new(core.clone(), worker.clone()));
            let jobs = Arc::new(JobCoordinator::start(
                core.clone(),
                worker.clone(),
                app.handle().clone(),
            )?);
            app.manage(AppState {
                core: core.clone(),
                recording,
                worker: worker.clone(),
                jobs,
            });

            // Full runtime hashing remains mandatory before execution, but it
            // no longer delays the first WebView paint. `WorkerSupervisor`
            // waits on this same validation state if a user action arrives
            // before the background pass completes.
            let validation_core = core.clone();
            let validation_worker = worker.clone();
            let validation_app = app.handle().clone();
            if let Err(error) = thread::Builder::new()
                .name("saytrace-runtime-warmup".into())
                .spawn(move || {
                    let status = match validation_core.layout().ensure_runtime_validated() {
                        Ok(_) => {
                            if let Err(error) = validation_worker.start() {
                                log::warn!("background worker warmup failed: {error}");
                                "failed"
                            } else {
                                "ready"
                            }
                        }
                        Err(error) => {
                            log::error!("bundled runtime validation failed: {error}");
                            "failed"
                        }
                    };
                    let _ = validation_app.emit(
                        "startup://changed",
                        json!({"area":"runtime","status":status}),
                    );
                })
            {
                log::warn!("could not start runtime warmup thread: {error}");
                let _ = app.emit(
                    "startup://changed",
                    json!({"area":"runtime","status":"failed"}),
                );
            }

            let recovery_app = app.handle().clone();
            if let Err(error) = thread::Builder::new()
                .name("saytrace-startup-recovery".into())
                .spawn(move || {
                    let status = match core.recover_recordings_and_cleanup(recovery_scope) {
                        Ok(()) => "complete",
                        Err(error) => {
                            log::error!("background startup recovery failed: {error}");
                            "failed"
                        }
                    };
                    let _ = recovery_app.emit(
                        "startup://changed",
                        json!({"area":"recovery","status":status}),
                    );
                })
            {
                log::warn!("could not start startup recovery thread: {error}");
                let _ = app.emit(
                    "startup://changed",
                    json!({"area":"recovery","status":"failed"}),
                );
            }
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
