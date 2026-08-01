import {
  FileText,
  Library,
  Plus,
  Settings,
  UserRoundSearch,
} from "lucide-react";
import { formatMeetingDate } from "../lib/format";
import type { AppView, Meeting } from "../types";
import { BrandMark } from "./BrandMark";

interface SidebarProps {
  view: AppView;
  meetings: Meeting[];
  onNavigate: (view: AppView) => void;
  onNew: () => void;
}

function isMeetingSelected(view: AppView, meetingId: string) {
  return (
    (view.kind === "transcript" || view.kind === "recording") &&
    view.meetingId === meetingId
  );
}

export function Sidebar({
  view,
  meetings,
  onNavigate,
  onNew,
}: SidebarProps) {
  const recentMeetings = meetings.slice(0, 6);
  return (
    <aside className="sidebar" aria-label="Primary navigation">
      <div className="sidebar__brand">
        <BrandMark />
        <span>SayTrace</span>
      </div>

      <button className="new-transcription" type="button" onClick={onNew}>
        <Plus size={22} strokeWidth={1.7} />
        <span>New transcription</span>
      </button>

      <nav className="sidebar__nav" aria-label="Main">
        <button
          type="button"
          className={
            view.kind === "library" ||
            view.kind === "transcript" ||
            view.kind === "recording"
              ? "is-active"
              : ""
          }
          onClick={() => onNavigate({ kind: "library" })}
        >
          <Library size={22} strokeWidth={1.6} />
          <span>Library</span>
        </button>
        <button
          type="button"
          className={view.kind === "profiles" ? "is-active" : ""}
          onClick={() => onNavigate({ kind: "profiles" })}
        >
          <UserRoundSearch size={22} strokeWidth={1.6} />
          <span>Voice profiles</span>
        </button>
        <button
          type="button"
          className={view.kind === "settings" ? "is-active" : ""}
          onClick={() => onNavigate({ kind: "settings" })}
        >
          <Settings size={22} strokeWidth={1.6} />
          <span>Settings</span>
        </button>
      </nav>

      <div className="sidebar__recent">
        <p>Recent</p>
        <div className="recent-list">
          {recentMeetings.map((meeting) => (
            <button
              key={meeting.id}
              type="button"
              className={
                isMeetingSelected(view, meeting.id) ? "is-selected" : ""
              }
              onClick={() =>
                onNavigate({
                  kind:
                    meeting.status === "recording" ? "recording" : "transcript",
                  meetingId: meeting.id,
                })
              }
            >
              <FileText size={21} strokeWidth={1.6} aria-hidden="true" />
              <span>
                <strong>{meeting.title}</strong>
                <small>
                  {meeting.status === "recording"
                    ? "Recording"
                    : formatMeetingDate(meeting.createdAt, true)}
                  {meeting.status === "recording" ? " • 00:18:42" : ""}
                </small>
              </span>
            </button>
          ))}
        </div>
      </div>

      <div className="sidebar__privacy">
        <span className="privacy-dot" aria-hidden="true" />
        <span>Files stay on this device</span>
        <span className="privacy-info" aria-label="Local privacy information">
          i
        </span>
      </div>
    </aside>
  );
}
