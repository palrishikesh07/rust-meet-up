import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { getCurrentWindow } from "@tauri-apps/api/window";

import {
  listDevices,
  recordingStatus,
  startRecording,
  stopRecording,
  summarizeTranscript,
  type Device,
} from "./api";
import "./App.css";

// Window control functions
async function closeApp() {
  const active = getCurrentWindow();

  try {
    await stopRecording().catch(() => null);
  } catch (err) {
    console.error("Error stopping recording:", err);
  }

  await active.close();
}

// Utility functions
function hasQuestion(text: string): boolean {
  return /\?|\b(how|what|why|when|where|who|which)\b|question/i.test(text);
}

function formatClock(totalSeconds: number): string {
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${String(minutes).padStart(2, "0")}:${String(seconds).padStart(2, "0")}`;
}

function messageOf(err: unknown): string {
  if (typeof err === "string") return err;
  if (err instanceof Error) return err.message;
  return "Something went wrong";
}

function App() {
  const [devices, setDevices] = useState<Device[]>([]);
  const [source, setSource] = useState("");
  const [refreshingDevices, setRefreshingDevices] = useState(false);
  const [recording, setRecording] = useState(false);
  const [elapsed, setElapsed] = useState(0);
  const [activePath, setActivePath] = useState("");
  const [textPath, setTextPath] = useState("");
  const [transcript, setTranscript] = useState("");
  const [partial, setPartial] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [answer, setAnswer] = useState("");
  const [answering, setAnswering] = useState(false);

  const recordingRef = useRef(false);
  const stoppingRef = useRef(false);
  const summaryTask = useRef<Promise<unknown> | null>(null);
  const summaryGen = useRef(0);

  const appWindow = getCurrentWindow();

  useEffect(() => {
    const enableContentProtection = async () => {
      try {
        await appWindow.setContentProtected(true);
        console.log("Screen capture protection enabled");
      } catch (error) {
        console.error("Failed to enable content protection:", error);
      }
    };

    void enableContentProtection();
  }, [appWindow]);

  async function refreshDevices(showErrors = true) {
    setRefreshingDevices(true);

    try {
      // listDevices performs fresh native Windows device enumeration on every call.
      const listed = await listDevices();

      setDevices(listed.devices);
      setSource((current) => {
        // Preserve the user's selection if that endpoint is still available.
        if (current && listed.devices.some((device) => device.id === current)) {
          return current;
        }

        return listed.defaultSource || listed.devices[0]?.id || "";
      });

      if (showErrors) setError("");
    } catch (err) {
      if (showErrors) setError(messageOf(err));
    } finally {
      setRefreshingDevices(false);
    }
  }

  useEffect(() => {
    let cancelled = false;

    (async () => {
      try {
        const [listed, status] = await Promise.all([
          listDevices(),
          recordingStatus(),
        ]);

        if (cancelled) return;

        setDevices(listed.devices);
        setSource(listed.defaultSource);
        setRecording(status.recording);
        setElapsed(status.elapsedSecs);
        setActivePath(status.path ?? "");
        setTextPath(status.textPath ?? "");
        setTranscript(status.transcript);
        setPartial(status.partial);
        recordingRef.current = status.recording;
      } catch (err) {
        if (!cancelled) setError(messageOf(err));
      }
    })();

    return () => {
      cancelled = true;
    };
  }, []);

  // Windows may add/remove Bluetooth endpoints after the app has started.
  // Poll while idle so newly connected devices appear without restarting the app.
  useEffect(() => {
    if (recording) return;

    const timer = window.setInterval(() => {
      void refreshDevices(false);
    }, 10000);

    return () => window.clearInterval(timer);
  }, [recording]);

  useEffect(() => {
    if (!recording) return;

    const timer = window.setInterval(async () => {
      if (stoppingRef.current) return;

      try {
        const status = await recordingStatus();

        if (stoppingRef.current) return;

        if (!status.recording) {
          setRecording(false);
          recordingRef.current = false;
          return;
        }

        setRecording(true);
        setElapsed(status.elapsedSecs);
        setActivePath(status.path ?? "");
        setTextPath(status.textPath ?? "");
        setTranscript(status.transcript);
        setPartial(status.partial);
      } catch (err) {
        setError(messageOf(err));
      }
    }, 500);

    return () => window.clearInterval(timer);
  }, [recording]);

  useEffect(() => {
    recordingRef.current = recording;
  }, [recording]);

  function requestSummary(path: string, force = false) {
    if (!path) return Promise.resolve();
    if (!force && summaryTask.current) return summaryTask.current;

    const gen = ++summaryGen.current;

    setAnswering(true);
    setError("");
    setAnswer("");

    const task = summarizeTranscript(path, gen)
      .then((result) => {
        if (gen !== summaryGen.current) return;
        setAnswer(result.text);
      })
      .catch((err) => {
        if (gen !== summaryGen.current) return;
        summaryTask.current = null;
        setError(messageOf(err));
      })
      .finally(() => {
        if (gen === summaryGen.current) setAnswering(false);
      });

    summaryTask.current = task;
    return task;
  }

  useEffect(() => {
    const unlisten = listen<{ ticket: number; text: string }>(
      "answer-delta",
      (event) => {
        if (event.payload.ticket !== summaryGen.current) return;
        setAnswer((current) => current + event.payload.text);
      }
    );

    return () => {
      void unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    if (!recording || !textPath || summaryTask.current || !hasQuestion(transcript)) {
      return;
    }

    void requestSummary(textPath);
  }, [recording, textPath, transcript]);

  useEffect(() => {
    const unlistenClose = getCurrentWindow().onCloseRequested(async (event) => {
      if (!recordingRef.current) return;

      event.preventDefault();
      recordingRef.current = false;
      await getCurrentWindow().destroy();
    });

    // Keyboard shortcuts for closing
    const handleKeyDown = (event: KeyboardEvent) => {
      // Ctrl+Q (Windows/Linux) or Cmd+Q (macOS)
      if ((event.ctrlKey || event.metaKey) && event.key === "q") {
        event.preventDefault();
        void closeApp();
      }
      // Alt+F4 is handled by the OS/Tauri.
    };

    window.addEventListener("keydown", handleKeyDown);

    return () => {
      void unlistenClose.then((stop) => stop());
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, []);

  async function toggle() {
    setBusy(true);
    setError("");

    try {
      if (recording) {
        recordingRef.current = false;
        stoppingRef.current = true;
        setAnswering(true);

        const saved = await stopRecording();

        setRecording(false);
        setElapsed(0);
        setPartial("");
        setActivePath(saved.path);
        setTextPath(saved.textPath);

        const spoken = saved.transcript.trim();

        if (spoken) setTranscript(spoken);

        if (!saved.textPath || !spoken) {
          setError("The transcript was empty, so there was nothing to answer.");
        } else if (summaryTask.current) {
          await summaryTask.current;
        } else {
          await requestSummary(saved.textPath);
        }
      } else {
        const started = await startRecording(source);

        recordingRef.current = true;
        summaryTask.current = null;

        setAnswer("");
        setRecording(true);
        setElapsed(0);
        setTranscript("");
        setPartial("");
        setTextPath(started.textPath);
        setActivePath(started.path);
      }
    } catch (err) {
      recordingRef.current = false;
      setError(messageOf(err));

      const status = await recordingStatus().catch(() => null);

      if (status) {
        setRecording(status.recording);
        setElapsed(status.elapsedSecs);
        setActivePath(status.path ?? "");
      }
    } finally {
      stoppingRef.current = false;
      setAnswering(false);
      setBusy(false);
    }
  }

  const selected = devices.find((device) => device.id === source);

  const outputLabel =
    devices.length === 0
      ? "No playback devices found"
      : selected
        ? `${selected.label}${selected.isDefault ? " (current)" : ""}`
        : "Choose a playback output";

  return (
    <main className="app">
      {/* Header: recording controls are now in the top-right corner. */}
      <header className="app-header">
        <div className="app-brand">
          <span className="app-logo" aria-hidden="true">
            M
          </span>

          <div>
            <h2>Meet Capture</h2>
            <p className="hint">AI Meeting Assistant</p>
          </div>
        </div>

        <div className="header-record-controls">
          <div className="header-timer" aria-live="off">
            <span
              className={recording ? "status-dot active" : "status-dot"}
              aria-hidden="true"
            />
            <span>{formatClock(elapsed)}</span>
          </div>

          <button
            type="button"
            className={recording ? "record stop" : "record"}
            onClick={() => void toggle()}
            disabled={busy || (!recording && devices.length === 0)}
            aria-pressed={recording}
          >
            <span className="dot" />
            {busy ? "Please wait…" : recording ? "Stop" : "Record"}
          </button>
        </div>
      </header>

      {/* Answer panel */}
      <section className="panel answer-panel">
        <h2>Answer</h2>

        <div className="transcript answers answer-scroll">
          {answer ? (
            <div className="answer-content">
              <ReactMarkdown
                remarkPlugins={[remarkGfm]}
                components={{
                  code({ className, children, ...props }) {
                    const match = /language-(\w+)/.exec(className || "");

                    return (
                      <pre className="code-block">
                        <code
                          className={match ? `language-${match[1]}` : ""}
                          {...props}
                        >
                          {String(children).replace(/\n$/, "")}
                        </code>
                      </pre>
                    );
                  },
                }}
              >
                {answer}
              </ReactMarkdown>
            </div>
          ) : (
            <p className="hint">
              {answering
                ? "Answering the question…"
                : "The answer shows up here once a question is heard."}
            </p>
          )}
        </div>
      </section>

      {/* Audio source panel */}
      <section className="panel">
        <label id="output-label">Audio source</label>

        <OutputPicker
          label={outputLabel}
          devices={devices}
          source={source}
          disabled={recording || devices.length === 0}
          onChange={setSource}
        />

        <button
          type="button"
          className="refresh-devices"
          onClick={() => void refreshDevices(true)}
          disabled={recording || refreshingDevices}
          aria-label="Refresh audio device list"
          title="Check for newly connected Bluetooth and audio devices"
          style={{
            display: "inline-flex",
            alignItems: "center",
            justifyContent: "center",
            gap: "9px",
            marginTop: "10px",
            padding: "10px 16px",
            border: "1px solid rgba(255, 214, 10, 0.45)",
            borderRadius: "12px",
            background:
              recording || refreshingDevices
                ? "rgba(255, 255, 255, 0.06)"
                : "linear-gradient(135deg, #ffe45c 0%, #ffc928 100%)",
            color: "#292313",
            fontSize: "13px",
            fontWeight: 700,
            letterSpacing: "0.01em",
            boxShadow:
              recording || refreshingDevices
                ? "none"
                : "0 4px 14px rgba(255, 201, 40, 0.18)",
            cursor:
              recording || refreshingDevices ? "not-allowed" : "pointer",
            opacity: recording || refreshingDevices ? 0.65 : 1,
            transition:
              "transform 150ms ease, box-shadow 150ms ease, opacity 150ms ease",
          }}
        >
          <span aria-hidden="true" style={{ fontSize: "17px", lineHeight: 1 }}>
            {refreshingDevices ? "◌" : "↻"}
          </span>
          <span>
            {refreshingDevices ? "Checking devices…" : "Audio devices"}
          </span>
        </button>

        <p className="hint">
          {selected
            ? "Choose system audio to capture meeting sound from Bluetooth headphones or speakers, or choose a microphone/headset input to capture your voice."
            : "Connect your Bluetooth headset, headphones, speakers, or microphone, then select Refresh devices."}
        </p>

        {recording ? (
          <p className="hint recording-hint">
            Recording in progress. Audio and transcript are being saved.
          </p>
        ) : null}

        {activePath ? <p className="path">{activePath}</p> : null}
        {error ? <p className="error">{error}</p> : null}
      </section>

      {/* Transcript panel */}
      <section className="panel">
        <h2>Transcript</h2>

        <div className="transcript">
          {transcript || partial ? (
            <>
              {transcript ? <p>{transcript}</p> : null}
              {partial ? <p className="partial">{partial}</p> : null}
            </>
          ) : (
            <p className="hint">
              {recording
                ? "Listening for speech…"
                : "Lines appear here a few seconds after someone speaks, and the same text is saved next to the audio."}
            </p>
          )}
        </div>
      </section>

      <p className="fine">
        Record only conversations you are allowed to record. Files stay on this
        computer.
      </p>
    </main>
  );
}

interface OutputPickerProps {
  label: string;
  devices: Device[];
  source: string;
  disabled: boolean;
  onChange: (id: string) => void;
}

function OutputPicker({
  label,
  devices,
  source,
  disabled,
  onChange,
}: OutputPickerProps) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;

    const close = (event: MouseEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };

    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };

    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", onKey);

    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="picker" ref={root}>
      <button
        type="button"
        className="picker-button"
        aria-labelledby="output-label"
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        onClick={() => setOpen((value) => !value)}
      >
        <span>{label}</span>
        <span className="picker-chevron" aria-hidden="true" />
      </button>

      {open ? (
        <ul className="picker-menu" role="listbox" aria-labelledby="output-label">
          {devices.map((device) => {
            const text = `${device.label}${device.isDefault ? " (current)" : ""}`;
            const active = device.id === source;

            return (
              <li key={device.id} role="presentation">
                <button
                  type="button"
                  role="option"
                  aria-selected={active}
                  className={active ? "picker-option active" : "picker-option"}
                  onClick={() => {
                    onChange(device.id);
                    setOpen(false);
                  }}
                >
                  {text}
                </button>
              </li>
            );
          })}
        </ul>
      ) : null}
    </div>
  );
}

export default App;
