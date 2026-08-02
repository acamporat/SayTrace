import {
  CheckCircle2,
  ChevronRight,
  Info,
  Mic2,
  MoreVertical,
  Plus,
  Search,
  ShieldCheck,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useMemo, useState } from "react";
import { formatDuration, formatMeetingDate } from "../../lib/format";
import type { VoiceProfile } from "../../types";
import { SpeakerAvatar } from "../../components/SpeakerAvatar";

interface VoiceProfilesViewProps {
  profiles: VoiceProfile[];
  onCreate: (name: string) => Promise<VoiceProfile>;
  onDelete: (profileId: string) => void;
  onAddSample: (profileId: string) => void;
}

export function VoiceProfilesView({
  profiles,
  onCreate,
  onDelete,
  onAddSample,
}: VoiceProfilesViewProps) {
  const [search, setSearch] = useState("");
  const [expanded, setExpanded] = useState<string>();
  const [creating, setCreating] = useState(false);
  const [profileName, setProfileName] = useState("");
  const visible = useMemo(
    () =>
      profiles.filter((profile) =>
        profile.name.toLocaleLowerCase().includes(search.toLocaleLowerCase()),
      ),
    [profiles, search],
  );

  return (
    <main className="workspace page-workspace profiles-workspace">
      <header className="page-header">
        <div>
          <h1>Voice profiles</h1>
          <p>Remember familiar speakers without sending voice data anywhere.</p>
        </div>
        <button
          className="primary-button"
          type="button"
          onClick={() => setCreating(true)}
        >
          <Plus size={19} /> Create profile
        </button>
      </header>

      {creating ? (
        <form
          className="create-profile-form"
          onSubmit={(event) => {
            event.preventDefault();
            const name = profileName.trim();
            if (!name) return;
            void onCreate(name).then(() => {
              setProfileName("");
              setCreating(false);
            });
          }}
        >
          <div>
            <strong>Create voice profile</strong>
            <p>Name the person now; add confirmed clean samples afterward.</p>
          </div>
          <input
            autoFocus
            aria-label="New voice profile name"
            placeholder="Speaker name"
            value={profileName}
            onChange={(event) => setProfileName(event.target.value)}
          />
          <button className="primary-button" type="submit" disabled={!profileName.trim()}>
            Create
          </button>
          <button
            className="secondary-button"
            type="button"
            onClick={() => {
              setProfileName("");
              setCreating(false);
            }}
          >
            Cancel
          </button>
        </form>
      ) : null}

      <div className="profile-privacy">
        <ShieldCheck size={21} />
        <div>
          <strong>Encrypted on this Windows account</strong>
          <p>
            Voice signatures are protected with Windows DPAPI and never leave
            this device.
          </p>
        </div>
      </div>

      <label className="profile-search">
        <Search size={18} />
        <input
          aria-label="Search voice profiles"
          placeholder="Search profiles"
          value={search}
          onChange={(event) => setSearch(event.target.value)}
        />
      </label>

      <section className="profile-list" aria-label="Voice profiles">
        {visible.map((profile) => (
          <article
            key={profile.id}
            className={expanded === profile.id ? "is-expanded" : ""}
          >
            <SpeakerAvatar
              initials={profile.initials}
              color={profile.color}
              size="large"
            />
            <div className="profile-list__identity">
              <strong>{profile.name}</strong>
              <span
                className={
                  profile.status === "ready"
                    ? "profile-status profile-status--ready"
                    : "profile-status profile-status--warning"
                }
              >
                {profile.status === "ready" ? (
                  <CheckCircle2 size={14} />
                ) : (
                  <TriangleAlert size={14} />
                )}
                {profile.status === "ready" ? "Ready to match" : "Needs samples"}
              </span>
            </div>
            <div className="profile-stat">
              <small>Clean speech</small>
              <strong>{formatDuration(profile.sampleDurationMs, false)}</strong>
            </div>
            <div className="profile-stat">
              <small>Samples</small>
              <strong>{profile.sampleCount}</strong>
            </div>
            <div className="profile-stat profile-stat--wide">
              <small>Last recognized</small>
              <strong>{formatMeetingDate(profile.lastUsedAt)}</strong>
            </div>
            <button
              className="icon-button"
              type="button"
              aria-label={`Profile options for ${profile.name}`}
              onClick={() =>
                setExpanded((current) =>
                  current === profile.id ? undefined : profile.id,
                )
              }
            >
              <MoreVertical size={19} />
            </button>
            {expanded === profile.id ? (
              <div className="profile-list__expanded">
                <span>
                  <Mic2 size={17} /> Profiles become ready after at least 10
                  seconds of clean, user-confirmed speech.
                </span>
                <button
                  type="button"
                  onClick={() => onAddSample(profile.id)}
                >
                  Add voice sample <ChevronRight size={16} />
                </button>
                <button
                  className="danger-link"
                  type="button"
                  onClick={() => onDelete(profile.id)}
                >
                  <Trash2 size={16} /> Delete profile
                </button>
              </div>
            ) : null}
          </article>
        ))}
      </section>

      <div className="profile-note">
        <Info size={18} />
        <p>
          Automatic matches never retrain a profile. Clean voice samples are
          saved when you confirm a speaker or rename an unknown speaker.
        </p>
      </div>
    </main>
  );
}
