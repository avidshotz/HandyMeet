import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Copy, Mic, Square, Trash2, Download, Users } from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  events,
  type MeetingListItem,
  type MeetingRecord,
  type MeetingUtterance,
  type SystemAudioDevice,
} from "@/bindings";
import { Button } from "../ui/Button";
import { Input } from "../ui/Input";

const SPEAKER_COLORS = [
  "bg-logo-primary/80",
  "bg-sky-600",
  "bg-amber-600",
  "bg-emerald-600",
  "bg-violet-600",
  "bg-rose-600",
  "bg-teal-600",
];

function speakerColor(speakerId: string): string {
  if (speakerId === "you") {
    return "bg-logo-primary";
  }
  let hash = 0;
  for (const char of speakerId) {
    hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  }
  return SPEAKER_COLORS[hash % SPEAKER_COLORS.length];
}

function formatClock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(total / 60)
    .toString()
    .padStart(2, "0");
  const seconds = (total % 60).toString().padStart(2, "0");
  return `${minutes}:${seconds}`;
}

function unwrap<T>(result: { status: "ok"; data: T } | { status: "error"; error: string }): T {
  if (result.status === "error") {
    throw new Error(result.error);
  }
  return result.data;
}

export const MeetingsView: React.FC = () => {
  const { t } = useTranslation();
  const [meetings, setMeetings] = useState<MeetingListItem[]>([]);
  const [active, setActive] = useState<MeetingRecord | null>(null);
  const [recording, setRecording] = useState(false);
  const [busy, setBusy] = useState(false);
  const [yourName, setYourName] = useState("You");
  const [title, setTitle] = useState("");
  const [systemDevices, setSystemDevices] = useState<SystemAudioDevice[]>([]);
  const [systemDevice, setSystemDevice] = useState("auto");
  const [speakerIdEnabled, setSpeakerIdEnabled] = useState(false);
  const [speakerIdModelReady, setSpeakerIdModelReady] = useState(false);
  const [editingSpeaker, setEditingSpeaker] = useState<string | null>(null);
  const [speakerDraft, setSpeakerDraft] = useState("");
  const transcriptEndRef = useRef<HTMLDivElement>(null);

  const loadList = useCallback(async () => {
    const list = unwrap(await commands.listMeetings());
    setMeetings(list);
  }, []);

  useEffect(() => {
    const boot = async () => {
      try {
        const [name, devices, current, rec, speakerId, modelReady] = await Promise.all([
          commands.getMeetingYourName(),
          commands.listSystemAudioDevices(),
          commands.getActiveMeeting(),
          commands.isMeetingRecording(),
          commands.getMeetingSpeakerIdEnabled(),
          commands.isMeetingSpeakerIdModelReady(),
        ]);
        setYourName(unwrap(name));
        setSystemDevices(unwrap(devices));
        setSpeakerIdEnabled(unwrap(speakerId));
        setSpeakerIdModelReady(unwrap(modelReady));
        const meeting = unwrap(current);
        if (meeting) {
          setActive(meeting);
        }
        setRecording(unwrap(rec));
        await loadList();
      } catch (error) {
        console.warn("Failed to load meetings", error);
      }
    };
    void boot();
  }, [loadList]);

  useEffect(() => {
    const unlistenUtterance = events.meetingUtteranceEvent.listen((event) => {
      const utterance = event.payload.utterance;
      setActive((current) => {
        if (!current || current.id !== utterance.meeting_id) {
          return current;
        }
        if (current.utterances.some((item) => item.id === utterance.id)) {
          return current;
        }
        const speakers = current.speakers.some((s) => s.speaker_id === utterance.speaker_id)
          ? current.speakers
          : [
              ...current.speakers,
              {
                speaker_id: utterance.speaker_id,
                display_name: utterance.speaker_name,
              },
            ];
        return {
          ...current,
          speakers,
          utterances: [...current.utterances, utterance],
        };
      });
    });
    const unlistenState = events.meetingStateEvent.listen((event) => {
      setActive(event.payload.meeting);
      setRecording(event.payload.meeting.status === "recording");
      void loadList();
    });
    return () => {
      void unlistenUtterance.then((fn) => fn());
      void unlistenState.then((fn) => fn());
    };
  }, [loadList]);

  useEffect(() => {
    transcriptEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [active?.utterances.length]);

  const toggleSpeakerId = async (checked: boolean) => {
    const previous = speakerIdEnabled;
    setSpeakerIdEnabled(checked);
    try {
      unwrap(await commands.setMeetingSpeakerIdEnabled(checked));
      if (checked && !speakerIdModelReady) {
        // The model downloads in the background; one delayed re-check is
        // enough to flip the hint once it's done without polling forever.
        setTimeout(() => {
          void commands.isMeetingSpeakerIdModelReady().then((result) => {
            setSpeakerIdModelReady(unwrap(result));
          });
        }, 4000);
      }
    } catch (error) {
      setSpeakerIdEnabled(previous);
      toast.error(t("meetings.speakerIdToggleFailed"), {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const startMeeting = async () => {
    setBusy(true);
    try {
      await commands.setMeetingYourName(yourName);
      const meeting = unwrap(
        await commands.startMeeting({
          title: title.trim() ? title.trim() : null,
          your_name: yourName.trim() ? yourName.trim() : null,
          system_audio_device: systemDevice,
        }),
      );
      setActive(meeting);
      setRecording(true);
      setTitle("");
      await loadList();
    } catch (error) {
      toast.error(t("meetings.startFailed"), {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setBusy(false);
    }
  };

  const stopMeeting = async () => {
    setBusy(true);
    try {
      const meeting = unwrap(await commands.stopMeeting());
      setActive(meeting);
      setRecording(false);
      await loadList();
    } catch (error) {
      toast.error(t("meetings.stopFailed"), {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setBusy(false);
    }
  };

  const openMeeting = async (id: number) => {
    try {
      setActive(unwrap(await commands.getMeeting(id)));
    } catch (error) {
      toast.error(t("meetings.loadFailed"));
    }
  };

  const renameSpeaker = async (speakerId: string) => {
    if (!active || !speakerDraft.trim()) {
      setEditingSpeaker(null);
      return;
    }
    try {
      const updated = unwrap(
        await commands.renameMeetingSpeaker(active.id, speakerId, speakerDraft.trim()),
      );
      setActive(updated);
      if (speakerId === "you") {
        setYourName(speakerDraft.trim());
      }
    } catch (error) {
      toast.error(t("meetings.renameFailed"));
    } finally {
      setEditingSpeaker(null);
    }
  };

  const deleteMeeting = async (id: number) => {
    try {
      unwrap(await commands.deleteMeeting(id));
      if (active?.id === id) {
        setActive(null);
      }
      await loadList();
    } catch (error) {
      toast.error(t("meetings.deleteFailed"), {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const copyNotes = async () => {
    if (!active) return;
    try {
      const markdown = unwrap(await commands.exportMeetingMarkdown(active.id));
      await navigator.clipboard.writeText(markdown);
      toast.success(t("meetings.copied"));
    } catch (error) {
      toast.error(t("meetings.exportFailed"));
    }
  };

  const saveNotes = async () => {
    if (!active) return;
    try {
      unwrap(await commands.saveMeetingMarkdown(active.id));
    } catch (error) {
      toast.error(t("meetings.exportFailed"), {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const utterances: MeetingUtterance[] = active?.utterances ?? [];
  const sortedMeetings = useMemo(() => meetings, [meetings]);

  return (
    <div className="flex w-full max-w-6xl min-h-[36rem] gap-3">
      <div className="flex w-56 shrink-0 flex-col min-h-0">
        <p className="text-sm font-semibold mb-2">{t("meetings.past")}</p>
        <div className="flex-1 overflow-y-auto space-y-1 pr-1">
          {sortedMeetings.length === 0 && (
            <p className="text-xs text-text/50">{t("meetings.emptyList")}</p>
          )}
          {sortedMeetings.map((meeting) => (
            <button
              key={meeting.id}
              className={`w-full text-start rounded-lg px-2 py-2 text-xs transition-colors ${
                active?.id === meeting.id
                  ? "bg-logo-primary/80"
                  : "hover:bg-mid-gray/20"
              }`}
              onClick={() => void openMeeting(meeting.id)}
            >
              <div className="font-medium truncate">{meeting.title}</div>
              <div className="opacity-70">
                {t("meetings.utteranceCount", { count: meeting.utterance_count })}
              </div>
            </button>
          ))}
        </div>
      </div>

      <div className="flex min-h-0 min-w-0 flex-1 flex-col gap-3">
        <div className="rounded-xl border border-mid-gray/20 bg-mid-gray/5 p-3 space-y-3">
          <div className="flex items-center gap-2">
            <Users className="w-4 h-4 shrink-0" />
            <p className="text-sm font-semibold">{t("meetings.title")}</p>
            {recording && (
              <span className="text-[10px] uppercase tracking-wide px-2 py-0.5 rounded-full bg-red-600 text-white">
                {t("meetings.live")}
              </span>
            )}
          </div>
          <p className="text-xs text-text/70">{t("meetings.subtitle")}</p>
          <div className="grid grid-cols-1 md:grid-cols-3 gap-2">
            <Input
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              placeholder={t("meetings.titlePlaceholder")}
              disabled={recording}
            />
            <Input
              value={yourName}
              onChange={(event) => setYourName(event.target.value)}
              placeholder={t("meetings.yourName")}
              disabled={recording}
            />
            <select
              className="px-3 py-2 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 rounded-md"
              value={systemDevice}
              disabled={recording}
              onChange={(event) => setSystemDevice(event.target.value)}
            >
              {systemDevices.map((device) => (
                <option key={device.name} value={device.name}>
                  {device.name === "auto"
                    ? t("meetings.systemAudioAuto")
                    : device.name}
                </option>
              ))}
            </select>
          </div>
          <label className="flex items-start gap-2 text-xs text-text/70">
            <input
              type="checkbox"
              className="mt-0.5"
              checked={speakerIdEnabled}
              disabled={recording}
              onChange={(event) => void toggleSpeakerId(event.target.checked)}
            />
            <span>
              <span className="font-medium text-text">{t("meetings.speakerIdToggle")}</span>
              {" — "}
              {t("meetings.speakerIdHint")}
              {speakerIdEnabled && !speakerIdModelReady && (
                <span className="italic"> {t("meetings.speakerIdDownloading")}</span>
              )}
            </span>
          </label>
          <div className="flex flex-wrap gap-2">
            {recording ? (
              <Button
                variant="danger"
                onClick={() => void stopMeeting()}
                disabled={busy}
                className="flex items-center gap-2"
              >
                <Square className="w-4 h-4" />
                {t("meetings.stop")}
              </Button>
            ) : (
              <Button
                onClick={() => void startMeeting()}
                disabled={busy}
                className="flex items-center gap-2"
              >
                <Mic className="w-4 h-4" />
                {t("meetings.start")}
              </Button>
            )}
            {active && (
              <>
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => void copyNotes()}
                  className="flex items-center gap-2"
                >
                  <Copy className="w-4 h-4" />
                  {t("meetings.copy")}
                </Button>
                <Button
                  variant="secondary"
                  size="sm"
                  onClick={() => void saveNotes()}
                  className="flex items-center gap-2"
                >
                  <Download className="w-4 h-4" />
                  {t("meetings.save")}
                </Button>
                {!recording && (
                  <Button
                    variant="danger-ghost"
                    size="sm"
                    onClick={() => void deleteMeeting(active.id)}
                    className="flex items-center gap-2"
                  >
                    <Trash2 className="w-4 h-4" />
                    {t("meetings.delete")}
                  </Button>
                )}
              </>
            )}
          </div>
        </div>

        {active ? (
          <div className="flex min-h-0 flex-1 gap-3">
            <div className="flex min-h-0 w-44 shrink-0 flex-col rounded-xl border border-mid-gray/20 p-3">
              <p className="text-xs font-semibold mb-2">{t("meetings.speakers")}</p>
              <div className="space-y-2 overflow-y-auto">
                {active.speakers.map((speaker) => (
                  <div key={speaker.speaker_id} className="text-xs">
                    <div className="flex items-center gap-2">
                      <span
                        className={`inline-block w-2 h-2 rounded-full ${speakerColor(speaker.speaker_id)}`}
                      />
                      {editingSpeaker === speaker.speaker_id ? (
                        <input
                          className="w-full bg-transparent border-b border-logo-primary text-xs"
                          value={speakerDraft}
                          autoFocus
                          onChange={(event) => setSpeakerDraft(event.target.value)}
                          onBlur={() => void renameSpeaker(speaker.speaker_id)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") {
                              void renameSpeaker(speaker.speaker_id);
                            }
                          }}
                        />
                      ) : (
                        <button
                          className="truncate text-start hover:text-logo-primary"
                          title={t("meetings.renameSpeaker")}
                          onClick={() => {
                            setEditingSpeaker(speaker.speaker_id);
                            setSpeakerDraft(speaker.display_name);
                          }}
                        >
                          {speaker.display_name}
                        </button>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </div>

            <div className="flex min-h-0 flex-1 flex-col gap-3">
              {(active.notes.summary ||
                active.notes.action_items.length > 0 ||
                active.notes.decisions.length > 0) && (
                <div className="rounded-xl border border-mid-gray/20 p-3 text-xs space-y-2 shrink-0 max-h-40 overflow-y-auto">
                  {active.notes.summary && (
                    <div>
                      <p className="font-semibold mb-1">{t("meetings.summary")}</p>
                      <p className="text-text/80">{active.notes.summary}</p>
                    </div>
                  )}
                  {active.notes.action_items.length > 0 && (
                    <div>
                      <p className="font-semibold mb-1">{t("meetings.actionItems")}</p>
                      <ul className="list-disc ps-4 space-y-0.5">
                        {active.notes.action_items.map((item) => (
                          <li key={item}>{item}</li>
                        ))}
                      </ul>
                    </div>
                  )}
                  {active.notes.decisions.length > 0 && (
                    <div>
                      <p className="font-semibold mb-1">{t("meetings.decisions")}</p>
                      <ul className="list-disc ps-4 space-y-0.5">
                        {active.notes.decisions.map((item) => (
                          <li key={item}>{item}</li>
                        ))}
                      </ul>
                    </div>
                  )}
                </div>
              )}

              <div className="min-h-0 flex-1 overflow-y-auto rounded-xl border border-mid-gray/20 p-3 space-y-3">
                {utterances.length === 0 && (
                  <p className="text-xs text-text/50">{t("meetings.waiting")}</p>
                )}
                {utterances.map((utterance) => (
                  <div key={utterance.id} className="text-sm">
                    <div className="flex items-center gap-2 text-xs mb-0.5">
                      <span
                        className={`inline-block w-2 h-2 rounded-full ${speakerColor(utterance.speaker_id)}`}
                      />
                      <span className="font-semibold">{utterance.speaker_name}</span>
                      <span className="opacity-50">{formatClock(utterance.start_ms)}</span>
                    </div>
                    <p className="ps-4 leading-relaxed">{utterance.text}</p>
                  </div>
                ))}
                <div ref={transcriptEndRef} />
              </div>
            </div>
          </div>
        ) : (
          <div className="flex-1 rounded-xl border border-dashed border-mid-gray/30 flex items-center justify-center text-sm text-text/50 p-6 text-center">
            {t("meetings.idleHint")}
          </div>
        )}
      </div>
    </div>
  );
};
