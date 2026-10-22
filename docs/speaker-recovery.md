# Speaker filtering and recovery

Click a speaker name or the body of a speaker card in the transcript's Speakers panel to show only that speaker's segments. This works for named, review, and unknown speakers. Search and replace follow the active filter. Choose **Show all speakers** to restore the full transcript. Playback still uses the original meeting timeline.

Final processing checks isolated short diarization fragments against aligned word boundaries. A fragment of at most 350 ms is reassigned only when it sits inside one word, the same speaker immediately surrounds it, and that speaker has substantially stronger word overlap. Separate brief interjections, overlapping speech, repeated clusters, and ambiguous cases are retained. Review warnings are recorded in the canonical processing artifact. This reduces one class of boundary error; it does not establish that every short speaker is false or guarantee voice recognition accuracy. Existing completed transcripts are not rewritten automatically.

## Save and restore

Settings → Storage and recovery → **Save speaker backup** writes a versioned JSON file containing voice profiles, encrypted voice samples, meeting speaker labels/matches, and merge rules. Keep a copy outside the app's data directory before upgrades. **Restore speaker backup** selects either an exported file or an automatic snapshot. Use a new filename when saving; existing backup files are protected from overwrite.

Automatic speaker snapshots are taken before and after speaker changes, when processing is queued/completes, and at startup. The latest 50 are retained in `%LOCALAPPDATA%\com.localtranscript.desktop\backups\speakers`. A failed pre-change backup prevents the change; a failed after-change or startup snapshot is logged without pretending the completed change failed.

Voice samples remain protected by Windows DPAPI. Restore on the same Windows installation under the same account. Meeting assignments and samples require their original meeting records: missing meetings are skipped. Use a library/database backup as well when moving or restoring the meeting library. This is not a cross-machine biometric export.

Restoration validates the format and encrypted samples before a transaction. Existing profiles and matching speaker identities are updated; unrelated records remain. Saved merges are applied to existing transcript assignments. Conflicting speaker IDs and cross-meeting references are rejected. A pre-restore snapshot provides a recovery point, and failed transactions roll back.
