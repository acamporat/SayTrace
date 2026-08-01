# Design Fidelity Ledger

Accepted references:

- `docs/design/transcript-workspace-concept.png`
- `docs/design/live-recording-concept.png`

Final implementation captures:

- `docs/qa/transcript-final-1920x1080.jpg`
- `docs/qa/recording-final-1920x1080.jpg`
- `docs/qa/transcript-final-1280x720.jpg`
- `docs/qa/recording-final-1280x720.jpg`

The final QA pass used the Codex in-app browser against the deterministic Vite
demonstration renderer, not a packaged Tauri/WASAPI session.
The browser viewport was explicitly set to 1920×1080 and 1280×720, then reset
after capture. The accepted PNG concepts and all four final implementation
captures were inspected at original detail with the local image viewer.

| Comparison point | Accepted concept | Final implementation | Result |
| --- | --- | --- | --- |
| Window chrome | Compact Windows title bar with SayTrace mark | Matching custom title bar, app mark, and window controls | Match |
| Navigation | Deep-ink fixed rail, blue create action, Library and recent meeting selected | Same hierarchy, widths, active states, privacy footer, and solid-color treatment | Match |
| Transcript header | Meeting title, date/time, search, and offline state | Same controls, spacing, and responsive icon-only offline state with an accessible label | Match |
| Processing strip | Four completed stages joined by blue rules | Same four stages and completion treatment | Match |
| Playback | Transport controls, elapsed time, waveform, speed, volume, and overflow menu | Same control order; media uses an asset-ID URL and Range requests; the waveform-shaped timeline seeks correctly | Functional match; see deviation |
| Transcript canvas | Open white reading area, timestamp/speaker columns, selected turn, bottom tools | Same layout, editable turns, search marks, bookmarks, and auto-scroll | Match |
| Speaker panel | Three color-coded cards, rename/merge/review, profile creation | Same cards and actions; categorical states and explicit accept/keep-unknown actions replace percentages | Intentional plan change |
| Recording toolbar | Timer, stop/finalize, pause, two isolated source rows and meters | Same controls, order, proportions, labels, and event-driven meter state; device and monitor controls are visibly locked after capture starts | Intentional behavior |
| Live draft | `You` for isolated microphone speech and anonymous remote speakers | Same provisional labels, unsettled-suffix indicator, and draft warning | Match |
| Recording health | Source-active states, local-save state, free space, markers | Same health panel and marker workflow | Match |
| Responsive layout | Desktop workspace remains usable at the smaller release viewport | 1280×720 has a 1280-pixel document width, no horizontal overflow, and no clipped core controls | Match |

## Above-the-fold copy check

Transcript copy matches the approved concept for the product name, meeting title,
date/time, search hint, offline state, pipeline stages, speaker names, voice-profile
action, and privacy footer. Recording copy matches for the source names, timer
actions, `Live draft`, provisional speaker labels, health states, marker labels, and
the final-processing warning. The timer itself is live, so its captured value is
expected to differ from the static concept.

Intentional copy and behavior differences from the early visual:

- Percentage confidence was replaced by `Matched`, `Review`, and `Unknown`, as
  required by the approved implementation plan.
- The informational note now explains strict name-assignment thresholds instead of
  describing a percentage as confidence.
- A review candidate exposes `Accept <name>` and `Keep unknown` instead of treating
  an embedding similarity as a probability.
- The bottom action is `Flag for review`, not `Add comment`, because v1 implements
  durable review flags and does not claim a collaboration/comment system.
- Recording device selectors and monitor-level sliders are locked after capture
  begins; source choices are made before recording and Windows owns monitor levels.

The current waveform bars are a deterministic, seekable timeline visualization.
They are not yet amplitude peaks extracted from the selected media. This is the one
remaining functional visual gap from a literal media waveform and is not represented
as completed elsewhere in the release documentation.

## Interaction and accessibility checks

- Ctrl+F moves focus to transcript search and matching text is highlighted.
- Transcript text can be edited without replacing immutable model text in the
  persistence contract; revision conflicts roll back the optimistic edit.
- Simulated play/pause, pause/resume recording, marker creation, and the
  stop/finalize processing transition were exercised in the in-app browser.
- Speaker candidate acceptance, inline rename with focus-leave commit, and the merge
  picker were exercised in the in-app browser.
- Model Setup component tests cover automatic bundled-runtime detection, the
  incomplete-install repair state, determinate per-model and overall progress,
  accessible install errors, and retry-safe token retention.
- Recording component coverage exercises the default live draft and the explicit
  per-recording captions-off path; final processing remains sourced from saved media.
- Library, Voice Profiles, Settings, and Model Setup were reached by keyboard-safe,
  named controls.
- The 1280×720 icon-only offline control retains the accessible name
  `Offline mode`.

## Verification conclusion

The demonstration renderer is faithful to the accepted transcript and recording
concepts at both required viewports for typography, palette, proportions, control
order, transcript and speaker hierarchy, recording state, and responsive behavior.
This comparison is visual/interaction evidence only; it does not validate packaged
media, WASAPI, or ML behavior. The differences above are deliberate plan decisions
or the explicitly disclosed waveform-amplitude follow-up; no other material layout
or copy mismatch was found in the final side-by-side inspection.
