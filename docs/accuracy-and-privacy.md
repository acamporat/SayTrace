# Accuracy and privacy policy

## Speaker identity

The product favors precision over coverage:

- Diarization clusters remain anonymous and immutable internally.
- A known name is accepted only after configured absolute-score and
  runner-up-margin gates for the pinned embedding model and source type.
- A usable profile requires at least 30 seconds of clean non-overlapping speech from at least three samples.
- Automatic matches never update a profile.
- Uncertain or short clusters remain `Unknown`.
- Similarity values are not displayed as probabilities.

The release calibration target is at least 99% precision for automatically accepted
names and less than 1% false acceptance of unknown speakers. The checked-in
thresholds are conservative starting values, not evidence that this gate has been
met; a release must calibrate and validate them on the consented benchmark. A
low-confidence correct person may remain unknown; this is intentional.

## Transcript review

The app preserves:

- immutable model text and word timing;
- user-edited text as a separate revision;
- anonymous cluster identity;
- friendly speaker assignment and whether it came from a model or a person;
- turn-level review flags for overlap and uncertain speaker identity;
- job-level warnings for alignment degradation and decode fallback.

Reprocessing may replace model output but must reapply compatible user corrections and never silently discard them.

## Live versus final

Live captions are explicitly provisional. They use a faster English model, rolling context, and replaceable suffixes. Live speaker labels are source-derived or session-local anonymous labels.

Finalization:

1. unloads the live model;
2. starts from authoritative recorded media;
3. runs the accuracy pipeline independently;
4. replaces all draft text, timing, and clusters.

The canonical final result must be identical with live captions enabled or disabled.
The UI exposes this per recording, but the equality claim remains a release test
gate until it is exercised against the full pinned runtime and models.

## Network boundary

A production release includes an Authenticode-signed processing runtime in the
normal SayTrace installer. Explicit first-run model setup may then
download revision-pinned, SHA-256-verified model files once the user accepts the
Community-1 terms. After setup:

- inference performs no network requests;
- implicit token discovery is disabled;
- Hugging Face offline mode is enabled;
- pyannote metrics and Hugging Face telemetry are disabled;
- the application has no background or in-app release update feed;
- local model-status refreshes do not contact model hosts; and
- network access is enabled only for explicit model provisioning.

Offline acceptance testing blocks DNS and outbound traffic and removes the setup token before a complete transcription run.
