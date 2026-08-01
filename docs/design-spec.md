# SayTrace design specification

The accepted concept images are checked into the repository and are the source of truth for implementation:

- Transcript workspace: `docs/design/transcript-workspace-concept.png`
- Live recording: `docs/design/live-recording-concept.png`

## Visual system

- Color temperature: true white workspace, never cream or tinted white.
- Navigation: deep ink `#071b2e`, selected surface `#173755`, primary action `#0875ee`.
- Primary UI: cobalt `#096fd8`; success `#20a866`; recording `#e82929`.
- Speaker accents: green `#42aa79`, violet `#7c57cf`, teal `#169c98`, unknown gray `#6d7278`.
- Text: near-black `#15191d`; secondary `#68717a`; border `#d9dde2`.
- Typography: `"Segoe UI Variable", "Segoe UI", sans-serif`; 14px chrome, 16px transcript, 22px view titles.
- Geometry at 1680×940: 38px title bar, 296px left rail, flexible transcript canvas, approximately 290px right inspector.
- Container model: open transcript canvas and rails. Cards are limited to the small speaker, marker, status, and setup controls shown in the concepts.
- Borders and shadows: subtle one-pixel neutral borders; no gradients, glow, glass, or heavy elevation.
- Icons: consistent two-pixel outlined SVGs, 18–22px, with filled circles only for speaker avatars and status.

## Transcript workspace inventory

- Custom Windows title bar with app icon/name and minimize, maximize, and close controls.
- Left rail: `SayTrace`, `New transcription`, `Library`, `Voice profiles`, `Settings`, recent meetings, and `Files stay on this device`.
- Header: editable meeting title/date, transcript search, and `Offline`.
- Processing rail: `Preparing media`, `Transcribing`, `Aligning words`, `Identifying speakers`.
- Player: previous, play/pause, next, current/total time, waveform, speed, volume, and more menu.
- Canvas: timestamped speaker turns, selected-turn highlight, bookmark/comment actions, and auto-scroll.
- Inspector: speakers, categorical match state, rename/merge/review actions, and create-profile action.
- Intentional deviation: the concept's uncalibrated percentage confidence is replaced by `Matched`, `Review`, or `Unknown`.

## Recording inventory

- Header: editable `New meeting`, recording indicator/timer, `Stop and finalize`, and pause/resume.
- Inputs: microphone and system-audio rows with device selector, meter, and level control.
- Draft canvas: `Live draft`, provisional label disclosure, timestamped turns, and a replaceable current suffix.
- Inspector: microphone/system/local-save health, available disk, marker creation, and marker list.
- Footer: `Draft captions may change during final processing` and auto-scroll.

## Interaction rules

- All visible text and controls are code-native.
- Live captions are replaceable drafts; finalization replaces them from authoritative recorded media.
- Weak identity matches remain unknown and require review.
- Transcript text edits and speaker corrections update local state immediately and persist through the trusted Tauri API.
- Buttons, rows, search, playback, device selectors, markers, navigation, profile management, and exports must not be inert.
- Focus rings are visible; reduced-motion preferences disable nonessential animation.
- Desktop layouts are verified at 1920×1080 and 1280×720. Smaller widths may collapse the right inspector but must not turn the transcript into a card grid.
