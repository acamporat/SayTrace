import {
  CalendarDays,
  ChevronDown,
  FileAudio2,
  FileVideo2,
  FolderOpen,
  Import,
  ListFilter,
  Play,
  Search,
  Users,
} from "lucide-react";
import { useMemo, useState } from "react";
import { formatDuration, formatMeetingDate } from "../../lib/format";
import type { Meeting } from "../../types";

interface LibraryViewProps {
  meetings: Meeting[];
  onOpen: (meetingId: string) => void;
  onImport: () => void;
  onRecord: () => void;
}

export function LibraryView({
  meetings,
  onOpen,
  onImport,
  onRecord,
}: LibraryViewProps) {
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<"all" | "recording" | "import">("all");
  const [newestFirst, setNewestFirst] = useState(true);
  const visible = useMemo(
    () =>
      meetings
        .filter(
          (meeting) =>
            (filter === "all" || meeting.sourceType === filter) &&
            meeting.title
              .toLocaleLowerCase()
              .includes(search.toLocaleLowerCase()),
        )
        .sort((left, right) => {
          const order =
            new Date(right.createdAt).getTime() -
            new Date(left.createdAt).getTime();
          return newestFirst ? order : -order;
        }),
    [filter, meetings, newestFirst, search],
  );

  return (
    <main className="workspace page-workspace library-workspace">
      <header className="page-header">
        <div>
          <h1>Library</h1>
          <p>Your recordings and imported media, stored on this device.</p>
        </div>
        <div className="page-header__actions">
          <button className="secondary-button" type="button" onClick={onImport}>
            <Import size={18} />
            Import media
          </button>
          <button className="primary-button" type="button" onClick={onRecord}>
            <Play size={17} fill="currentColor" />
            Record meeting
          </button>
        </div>
      </header>

      <section className="library-toolbar" aria-label="Library filters">
        <label className="library-search">
          <Search size={19} />
          <input
            aria-label="Search library"
            placeholder="Search meetings"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
        </label>
        <div className="filter-tabs" role="group" aria-label="Media source">
          {[
            ["all", "All"],
            ["recording", "Recordings"],
            ["import", "Imports"],
          ].map(([value, label]) => (
            <button
              key={value}
              type="button"
              className={filter === value ? "is-active" : ""}
              onClick={() => setFilter(value as typeof filter)}
            >
              {label}
            </button>
          ))}
        </div>
        <button
          className="sort-button"
          type="button"
          aria-label={`Sort meetings ${newestFirst ? "oldest first" : "newest first"}`}
          onClick={() => setNewestFirst((current) => !current)}
        >
          <CalendarDays size={17} /> {newestFirst ? "Newest first" : "Oldest first"}{" "}
          <ChevronDown size={15} />
        </button>
      </section>

      <section className="meeting-list" aria-label="Meetings">
        <div className="meeting-list__header">
          <span>Name</span>
          <span>Recorded</span>
          <span>Duration</span>
          <span>Speakers</span>
          <span>Status</span>
        </div>
        {visible.length ? (
          visible.map((meeting) => (
            <article key={meeting.id}>
              <button
                className="meeting-list__open"
                type="button"
                onClick={() => onOpen(meeting.id)}
              >
                <span className="meeting-type-icon">
                  {meeting.sourceType === "import" ? (
                    <FileVideo2 size={21} />
                  ) : (
                    <FileAudio2 size={21} />
                  )}
                </span>
                <span className="meeting-list__name">
                  <strong>{meeting.title}</strong>
                  <small>
                    {meeting.sourceType === "import"
                      ? "Imported media"
                      : meeting.status === "recording"
                        ? "Recording now"
                        : "Local recording"}
                  </small>
                </span>
              </button>
              <time>{formatMeetingDate(meeting.createdAt, true)}</time>
              <span>{formatDuration(meeting.durationMs, false)}</span>
              <span className="meeting-list__speakers">
                <Users size={16} /> {meeting.speakerCount}
              </span>
              <span
                className={`meeting-status meeting-status--${meeting.status}`}
              >
                {meeting.status === "ready"
                  ? "Ready"
                  : meeting.status === "recording"
                    ? "Recording"
                    : meeting.status === "failed"
                      ? "Failed"
                      : "Processing"}
              </span>
            </article>
          ))
        ) : (
          <div className="empty-state">
            <FolderOpen size={34} />
            <h2>No meetings found</h2>
            <p>Try a different search or import a media file.</p>
          </div>
        )}
      </section>
      <footer className="library-footer">
        <span>{visible.length} meetings</span>
        <span>
          <ListFilter size={15} /> Showing {filter === "all" ? "all media" : filter}
        </span>
      </footer>
    </main>
  );
}
