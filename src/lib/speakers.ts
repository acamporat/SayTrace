import type { MeetingSpeaker } from "../types";

const LEGACY_UNKNOWN_NAME = /^unknown(?: speaker)?$/i;
const NUMBERED_UNKNOWN_NAME = /^speaker\s+(\d+)$/i;

function numberFromLabel(label?: string) {
  const trailingNumber = label?.match(/(\d+)$/)?.[1];
  const parsed = trailingNumber === undefined ? Number.NaN : Number(trailingNumber);
  return Number.isFinite(parsed) ? parsed + 1 : undefined;
}

function numberFromDisplayName(displayName: string) {
  const parsed = Number(displayName.trim().match(NUMBERED_UNKNOWN_NAME)?.[1]);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : undefined;
}

export function defaultUnknownSpeakerName(label?: string, fallback = 1) {
  return `Speaker ${numberFromLabel(label) ?? fallback}`;
}

export function normalizeUnknownSpeakerNames(
  speakers: MeetingSpeaker[],
): MeetingSpeaker[] {
  const anonymous = speakers.map(
    (speaker) =>
      speaker.state !== "Matched" &&
      (LEGACY_UNKNOWN_NAME.test(speaker.displayName.trim()) ||
        NUMBERED_UNKNOWN_NAME.test(speaker.displayName.trim())),
  );
  const used = new Set<number>();
  const assignments = new Map<number, number>();

  speakers.forEach((speaker, index) => {
    if (anonymous[index]) return;
    const number = numberFromDisplayName(speaker.displayName);
    if (number !== undefined) used.add(number);
  });

  speakers.forEach((speaker, index) => {
    if (!anonymous[index]) return;
    const preferred = numberFromLabel(speaker.label);
    if (preferred !== undefined && !used.has(preferred)) {
      assignments.set(index, preferred);
      used.add(preferred);
    }
  });

  speakers.forEach((speaker, index) => {
    if (!anonymous[index] || assignments.has(index)) return;
    const current = numberFromDisplayName(speaker.displayName);
    if (current !== undefined && !used.has(current)) {
      assignments.set(index, current);
      used.add(current);
    }
  });

  let fallback = 1;
  return speakers.map((speaker, index) => {
    if (!anonymous[index]) return speaker;
    let number = assignments.get(index);
    if (number === undefined) {
      while (used.has(fallback)) fallback += 1;
      number = fallback;
      used.add(number);
    }
    return {
      ...speaker,
      displayName: `Speaker ${number}`,
      initials: `S${number}`,
    };
  });
}
