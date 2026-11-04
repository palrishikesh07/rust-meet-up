mod capture;

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}, Mutex},
    thread::JoinHandle,
    time::Instant,
};
#[cfg(target_os = "windows")]
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};
use tauri::{Emitter, Manager};

#[derive(Default)] struct AppState { job: Mutex<Option<Job>> }
struct Job {
    child: Child,
    stdin: Option<ChildStdin>,
    capture_thread: Option<JoinHandle<Result<(), String>>>,
    stop_capture: Arc<AtomicBool>,
    native_loopback: bool,
    path: String,
    text_path: String,
    source: String,
    started: Instant,
}
#[derive(Clone, Serialize, Deserialize)] #[serde(rename_all="camelCase")] struct Device { id:String, label:String, detail:String, is_default:bool }
#[derive(Serialize, Deserialize)] #[serde(rename_all="camelCase")] struct DeviceList { downloads_dir:String, default_source:String, devices:Vec<Device> }
#[derive(Serialize, Deserialize)] #[serde(rename_all="camelCase")] struct RecordingFile { name:String, path:String, bytes:u64, modified:String, text_path:String, summary_path:String }
#[derive(Serialize, Deserialize)] #[serde(rename_all="camelCase")] struct ReadyMessage { status:String, path:String, source:String, text_path:String }
#[derive(Serialize)] #[serde(rename_all="camelCase")] struct RecordingStatus { recording:bool, path:Option<String>, text_path:Option<String>, source:Option<String>, elapsed_secs:u64, transcript:String, partial:String }
#[derive(Serialize)] #[serde(rename_all="camelCase")] struct SavedTake { path:String, bytes:u64, text_path:String, transcript:String }
#[derive(Serialize, Deserialize)] #[serde(rename_all="camelCase")] struct OpenAIResult { text:String, summary_path:String }
#[derive(Clone, Serialize)] struct AnswerDelta { ticket:u32, text:String }

fn downloads_dir()->Result<PathBuf,String>{ let p=dirs::download_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_else(||PathBuf::from(".")).join("Downloads")); fs::create_dir_all(&p).map_err(|e|format!("Cannot create Downloads folder: {e}"))?; Ok(p) }
fn key_path()->Result<PathBuf,String>{ let base=dirs::config_dir().ok_or("Could not locate application config directory")?; Ok(base.join("meet-capture").join("openai.key")) }
fn read_key()->Result<String,String>{ if let Ok(k)=std::env::var("OPENAI_API_KEY"){if !k.trim().is_empty(){return Ok(k.trim().into())}} let p=key_path()?; let k=fs::read_to_string(p).map_err(|_|"Add an OpenAI API key in Settings first".to_string())?; if k.trim().is_empty(){Err("Add an OpenAI API key in Settings first".into())}else{Ok(k.trim().into())} }
fn write_transcript(path:&str,text:&str)->Result<(),String>{ let p=Path::new(path); let name=p.file_name().and_then(|n|n.to_str()).unwrap_or(""); if !name.starts_with("meet-")||!name.ends_with(".txt"){return Err("Invalid transcript path".into())}; fs::write(p,text).map_err(|e|format!("Could not save transcript: {e}")) }
fn compact_error(s:&str)->String{ serde_json::from_str::<serde_json::Value>(s).ok().and_then(|v|v.get("error").and_then(|e|e.get("message")).and_then(|m|m.as_str()).map(str::to_string)).unwrap_or_else(||s.chars().take(500).collect()) }
fn transcribe(path:&str)->Result<String,String>{ let key=read_key()?; let bytes=fs::read(path).map_err(|e|format!("Could not read recording: {e}"))?; if bytes.is_empty(){return Err("Recording is empty".into())}; let part=reqwest::blocking::multipart::Part::bytes(bytes).file_name(Path::new(path).file_name().unwrap_or_default().to_string_lossy().to_string()).mime_str("audio/ogg").map_err(|e|e.to_string())?; let form=reqwest::blocking::multipart::Form::new().text("model","gpt-4o-mini-transcribe").text("language","en").text("response_format","json").part("file",part); let resp=reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(600)).build().map_err(|e|e.to_string())?.post("https://api.openai.com/v1/audio/transcriptions").bearer_auth(key).multipart(form).send().map_err(|e|format!("OpenAI transcription request failed: {e}"))?; let status=resp.status(); let body=resp.text().map_err(|e|e.to_string())?; if !status.is_success(){return Err(format!("OpenAI transcription returned {status}: {}",compact_error(&body)))} let v:serde_json::Value=serde_json::from_str(&body).map_err(|e|format!("Invalid transcription response: {e}"))?; let text=v["text"].as_str().unwrap_or("").trim().to_string(); if text.is_empty(){return Err("OpenAI returned an empty transcript".into())}; Ok(text) }
fn chat(question:&str,context:&str,stream:bool,app:Option<&tauri::AppHandle>,ticket:u32)->Result<String,String>{let key=read_key()?;
let body = serde_json::json!({
    "model": "gpt-4o-mini",
    "stream": stream,
    "messages": [
        {
            "role": "system",
            "content": "You are a helpful meeting and interview assistant. Answer the latest clear question found in the transcript or explicitly provided by the user. If the Question or task field contains a question, answer it directly. Otherwise, identify and answer the latest question in the transcript. Always answer technical and programming questions using general knowledge when helpful. Do not say 'No clear question detected' if a valid question is present. Provide clear explanations and code examples when useful. Do not invent meeting facts."
        },
        {
            "role": "user",
            "content": format!(
                "Transcript/context:\n{}\n\nQuestion or task:\n{}",
                context,
                question
            )
        }
    ]
});

let resp=reqwest::blocking::Client::new().post("https://api.openai.com/v1/chat/completions").bearer_auth(key).json(&body).timeout(std::time::Duration::from_secs(180)).send().map_err(|e|format!("OpenAI request failed: {e}"))?; let status=resp.status(); if !status.is_success(){let b=resp.text().unwrap_or_default();return Err(format!("OpenAI returned {status}: {}",compact_error(&b)))} if !stream {let v:serde_json::Value=resp.json().map_err(|e|e.to_string())?;return Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())} let mut answer=String::new(); use std::io::{BufRead,BufReader}; for line in BufReader::new(resp).lines(){let line=line.map_err(|e|e.to_string())?; if let Some(data)=line.strip_prefix("data: "){if data=="[DONE]"{break} if let Ok(v)=serde_json::from_str::<serde_json::Value>(data){if let Some(delta)=v["choices"][0]["delta"]["content"].as_str(){answer.push_str(delta);if let Some(a)=app{let _=a.emit("answer-delta",AnswerDelta{ticket,text:delta.into()});}}}}} Ok(answer) }

#[tauri::command]
fn list_devices() -> Result<DeviceList, String> {
    let dir = downloads_dir()?;
    #[cfg(target_os = "windows")]
    {
        // Re-enumerate endpoints on every command call so newly connected Bluetooth
        // playback devices can appear without restarting the application.
        let _ = wasapi::initialize_mta();
        let enumerator = DeviceEnumerator::new().map_err(|e| format!("Could not enumerate Windows audio devices: {e}"))?;
        let render_devices = enumerator.get_device_collection(&Direction::Render)
            .map_err(|e| format!("Could not list playback devices: {e}"))?;
        let default_id = enumerator.get_default_device(&Direction::Render).ok().and_then(|d| d.get_id().ok());
        let count = render_devices.get_nbr_devices().map_err(|e| e.to_string())?;
        let mut devices: Vec<Device> = Vec::new();
        for i in 0..count {
            let device = match render_devices.get_device_at_index(i) { Ok(d) => d, Err(_) => continue };
            let name = device.get_friendlyname().unwrap_or_else(|_| format!("Playback device {}", i + 1));
            let id = match device.get_id() { Ok(id) => id, Err(_) => continue };
            let is_default = default_id.as_deref() == Some(id.as_str());
            devices.push(Device {
                id: format!("wasapi-loopback:{id}"),
                label: format!("{name} — system audio"),
                detail: "Native Windows WASAPI loopback (captures playback audio)".into(),
                is_default,
            });
        }

        // Also expose microphone inputs through DirectShow when FFmpeg is available.
        if let Ok(output) = Command::new("ffmpeg")
            .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
            .output()
        {
            let log = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            for line in log.lines() {
                if !line.to_lowercase().contains("(audio)") { continue; }
                let Some(name) = line.split('"').nth(1).map(str::trim) else { continue };
                if name.is_empty() { continue; }
                let id = format!("dshow:{name}");
                if !devices.iter().any(|d| d.id == id) {
                    devices.push(Device { id, label: format!("{name} — microphone/input"), detail: "Windows microphone or audio input (FFmpeg DirectShow)".into(), is_default: false });
                }
            }
        }
        let default_source = devices.iter().find(|d| d.is_default).map(|d| d.id.clone())
            .or_else(|| devices.first().map(|d| d.id.clone())).unwrap_or_default();
        if devices.is_empty() {
            return Err("Windows did not report any active playback or microphone devices. Connect the Bluetooth device, confirm it is connected in Windows Settings > System > Sound, then click Refresh devices.".into());
        }
        Ok(DeviceList { downloads_dir: dir.to_string_lossy().into(), default_source, devices })
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(DeviceList {
            downloads_dir: dir.to_string_lossy().into(), default_source: "default".into(),
            devices: vec![Device { id: "default".into(), label: "Default system audio".into(), detail: "Platform capture backend needs configuration".into(), is_default: true }],
        })
    }
}

#[cfg(target_os = "windows")]
fn run_wasapi_loopback(device_id: String, stop: Arc<AtomicBool>, mut raw_audio: ChildStdin) -> Result<(), String> {
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| format!("WASAPI device enumeration failed: {e}"))?;
    let device = enumerator.get_device(&device_id).map_err(|e| format!("Selected playback device is unavailable: {e}"))?;
    let mut client = device.get_iaudioclient().map_err(|e| format!("Could not open playback device: {e}"))?;
    // Ask WASAPI to convert the endpoint mix format to stereo 48 kHz float samples.
    let format = WaveFormat::new(32, 32, &SampleType::Float, 48_000, 2, None);
    let (default_period, _) = client.get_device_period().map_err(|e| format!("Could not read playback period: {e}"))?;
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: default_period };
    // A render endpoint initialized with Direction::Capture enables loopback capture.
    client.initialize_client(&format, &Direction::Capture, &mode)
        .map_err(|e| format!("Could not initialize WASAPI loopback: {e}"))?;
    let event = client.set_get_eventhandle().map_err(|e| format!("Could not create audio event: {e}"))?;
    let capture = client.get_audiocaptureclient().map_err(|e| format!("Could not access loopback samples: {e}"))?;
    client.start_stream().map_err(|e| format!("Could not start WASAPI loopback: {e}"))?;
    let mut bytes = std::collections::VecDeque::<u8>::new();
    let result = (|| -> Result<(), String> {
        while !stop.load(Ordering::Relaxed) {
            // A short timeout keeps the Stop button responsive even during silence.
            let _ = event.wait_for_event(100);
            loop {
                if stop.load(Ordering::Relaxed) { break; }
                let frames = capture.get_next_packet_size().map_err(|e| format!("WASAPI packet read failed: {e}"))?.unwrap_or(0);
                if frames == 0 { break; }
                capture.read_from_device_to_deque(&mut bytes).map_err(|e| format!("Could not read playback audio: {e}"))?;
                let (first, second) = bytes.as_slices();
                if !first.is_empty() { raw_audio.write_all(first).map_err(|e| format!("Could not send audio to encoder: {e}"))?; }
                if !second.is_empty() { raw_audio.write_all(second).map_err(|e| format!("Could not send audio to encoder: {e}"))?; }
                bytes.clear();
            }
        }
        Ok(())
    })();
    let _ = client.stop_stream();
    drop(raw_audio); // EOF tells FFmpeg to finalize the OGG/Opus container.
    result
}

#[tauri::command] fn list_recordings()->Result<Vec<RecordingFile>,String>{let dir=downloads_dir()?;let mut out=vec![];for e in fs::read_dir(&dir).map_err(|e|e.to_string())?{let e=e.map_err(|e|e.to_string())?;let p=e.path();let n=e.file_name().to_string_lossy().to_string();if !n.starts_with("meet-")||!n.ends_with(".ogg"){continue}let m=e.metadata().map_err(|e|e.to_string())?;let text=p.with_extension("txt");let summary=p.with_extension("summary.txt");out.push(RecordingFile{name:n,path:p.to_string_lossy().into(),bytes:m.len(),modified:m.modified().ok().map(|t|format!("{:?}",t)).unwrap_or_default(),text_path:if text.exists(){text.to_string_lossy().into()}else{String::new()},summary_path:if summary.exists(){summary.to_string_lossy().into()}else{String::new()}})}out.sort_by(|a,b|b.name.cmp(&a.name));Ok(out)}
#[tauri::command] fn recording_status(state:tauri::State<'_,AppState>)->RecordingStatus{let g=state.job.lock().unwrap();if let Some(j)=g.as_ref(){RecordingStatus{recording:true,path:Some(j.path.clone()),text_path:Some(j.text_path.clone()),source:Some(j.source.clone()),elapsed_secs:j.started.elapsed().as_secs(),transcript:String::new(),partial:String::new()}}else{RecordingStatus{recording:false,path:None,text_path:None,source:None,elapsed_secs:0,transcript:String::new(),partial:String::new()}}}
#[tauri::command]
fn start_recording(state: tauri::State<'_, AppState>, source: Option<String>) -> Result<ReadyMessage, String> {
    let mut guard = state.job.lock().map_err(|_| "Recording state unavailable")?;
    if guard.is_some() { return Err("A recording is already running".into()); }
    let devices = list_devices()?;
    let src = source.filter(|s| !s.is_empty()).unwrap_or(devices.default_source);
    if src.is_empty() { return Err("No audio source selected".into()); }
    let dir = downloads_dir()?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("meet-{stamp}.ogg"));
    let text_path = dir.join(format!("meet-{stamp}.txt"));
    let stop_capture = Arc::new(AtomicBool::new(false));
    let mut capture_thread: Option<JoinHandle<Result<(), String>>> = None;
    let mut stdin_for_job: Option<ChildStdin> = None;
    let native_loopback = src.starts_with("wasapi-loopback:");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "warning"]);

    #[cfg(target_os = "windows")]
    let mut child = if let Some(device_id) = src.strip_prefix("wasapi-loopback:") {
        // WASAPI runs natively in Rust; FFmpeg only encodes the raw samples piped to stdin.
        cmd.args(["-f", "f32le", "-ar", "48000", "-ac", "2", "-i", "pipe:0", "-vn", "-c:a", "libopus", "-b:a", "96k", "-application", "audio", "-vbr", "on"]);
        let mut child = cmd.arg(&path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
            .spawn().map_err(|e| format!("Could not start FFmpeg encoder: {e}. Ensure FFmpeg and libopus are installed and on PATH."))?;
        let raw_audio = child.stdin.take().ok_or("Could not connect audio encoder stdin")?;
        let stop = Arc::clone(&stop_capture);
        let device_id = device_id.to_string();
        capture_thread = Some(std::thread::Builder::new().name("wasapi-loopback-capture".into()).spawn(move || run_wasapi_loopback(device_id, stop, raw_audio)).map_err(|e| format!("Could not start WASAPI capture thread: {e}"))?);
        child
    } else if let Some(device_name) = src.strip_prefix("dshow:") {
        cmd.args(["-f", "dshow", "-i"]).arg(format!("audio={device_name}"))
            .args(["-vn", "-c:a", "libopus", "-b:a", "96k", "-application", "audio", "-vbr", "on"]);
        let child = cmd.arg(&path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
            .spawn().map_err(|e| format!("Could not start FFmpeg microphone capture: {e}. Ensure FFmpeg is installed and on PATH."))?;
        child
    } else { return Err("Unsupported audio source. Refresh the device list and select a playback or microphone device.".into()); };

    #[cfg(not(target_os = "windows"))]
    let mut child = {
        cmd.args(["-f", "avfoundation", "-i", ":0", "-vn", "-c:a", "libopus", "-b:a", "96k"]);
        cmd.arg(&path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped())
            .spawn().map_err(|e| format!("Could not start FFmpeg: {e}. Ensure FFmpeg is installed and available on PATH."))?
    };

    if !native_loopback {
        stdin_for_job = Some(child.stdin.take().ok_or("Could not connect to recorder stdin")?);
    }
    let ready = ReadyMessage { status: "recording".into(), path: path.to_string_lossy().into(), source: src.clone(), text_path: text_path.to_string_lossy().into() };
    *guard = Some(Job { child, stdin: stdin_for_job, capture_thread, stop_capture, native_loopback, path: ready.path.clone(), text_path: ready.text_path.clone(), source: src, started: Instant::now() });
    Ok(ready)
}

#[tauri::command]
async fn stop_recording(state: tauri::State<'_, AppState>) -> Result<SavedTake, String> {
    let job = state.job.lock().map_err(|_| "Recording state unavailable")?.take().ok_or("Nothing is recording")?;
    tauri::async_runtime::spawn_blocking(move || {
        let mut j = job;
        if j.native_loopback {
            j.stop_capture.store(true, Ordering::Relaxed);
            if let Some(handle) = j.capture_thread.take() {
                match handle.join() {
                    Ok(Ok(())) => {},
                    Ok(Err(e)) => { let _ = j.child.kill(); let _ = j.child.wait(); return Err(e); },
                    Err(_) => { let _ = j.child.kill(); let _ = j.child.wait(); return Err("WASAPI capture thread stopped unexpectedly".into()); }
                }
            }
        } else if let Some(mut stdin) = j.stdin.take() {
            stdin.write_all(b"q\n").map_err(|e| format!("Could not stop recorder: {e}"))?;
            drop(stdin);
        }
        let status = j.child.wait().map_err(|e| format!("Could not finish audio encoder: {e}"))?;
        if !status.success() {
            let err = j.child.stderr.take().and_then(|mut e| { let mut s = String::new(); e.read_to_string(&mut s).ok().map(|_| s) }).unwrap_or_default();
            return Err(if err.trim().is_empty() { format!("Recorder exited with {status}") } else { err });
        }
        let meta = fs::metadata(&j.path).map_err(|e| format!("Recording was not saved: {e}"))?;
        if meta.len() == 0 { return Err("Recording file is empty. Check the selected audio device and play some audio while recording.".into()); }
        let transcript = transcribe(&j.path)?;
        write_transcript(&j.text_path, &transcript)?;
        Ok(SavedTake { path: j.path, bytes: meta.len(), text_path: j.text_path, transcript })
    }).await.map_err(|e| e.to_string())?
}

#[tauri::command] fn save_openai_key(key:String)->Result<(),String>{let p=key_path()?;let parent=p.parent().ok_or("Invalid key path")?;fs::create_dir_all(parent).map_err(|e|e.to_string())?;let key=key.trim();if key.is_empty(){if p.exists(){fs::remove_file(p).map_err(|e|e.to_string())?}return Ok(())}fs::write(&p,format!("{key}\n")).map_err(|e|format!("Could not save API key: {e}"))}
#[derive(Serialize)] struct KeyStatus{saved:bool}
#[tauri::command] fn openai_key_status()->KeyStatus{KeyStatus{saved:read_key().is_ok()}}
#[tauri::command] fn save_transcript(path:String,text:String)->Result<(),String>{write_transcript(&path,&text)}
#[tauri::command] async fn summarize_transcript(app:tauri::AppHandle,path:String,ticket:u32)->Result<OpenAIResult,String>{tauri::async_runtime::spawn_blocking(move||{let transcript=fs::read_to_string(&path).map_err(|e|format!("Could not read transcript: {e}"))?;let answer=chat("Summarize the meeting in concise bullets. Include key decisions, technical points, action items, and unanswered questions.",&transcript,true,Some(&app),ticket)?;let summary_path=Path::new(&path).with_extension("summary.txt").to_string_lossy().to_string();fs::write(&summary_path,&answer).map_err(|e|e.to_string())?;Ok(OpenAIResult{text:answer,summary_path})}).await.map_err(|e|e.to_string())?}
#[tauri::command] async fn answer_question(app:tauri::AppHandle,question:String,context:String,path:String)->Result<OpenAIResult,String>{tauri::async_runtime::spawn_blocking(move||{let transcript=if !path.trim().is_empty(){fs::read_to_string(&path).unwrap_or_default()}else{String::new()};let full=if transcript.trim().is_empty(){context}else{format!("{transcript}\n\n{context}")};let text=chat(&question,&full,false,Some(&app),0)?;let summary_path=if !path.trim().is_empty(){Path::new(&path).with_extension("answers.txt").to_string_lossy().to_string()}else{downloads_dir()?.join("meet-answers.txt").to_string_lossy().to_string()};use std::fs::OpenOptions;let mut f=OpenOptions::new().create(true).append(true).open(&summary_path).map_err(|e|e.to_string())?;writeln!(f,"\nQ: {question}\nA: {text}\n").map_err(|e|e.to_string())?;Ok(OpenAIResult{text,summary_path})}).await.map_err(|e|e.to_string())?}

#[cfg_attr(mobile,mobile_entry_point)] pub fn run(){tauri::Builder::default().manage(AppState::default()).plugin(tauri_plugin_opener::init()).setup(|app|{if let Some(w)=app.get_webview_window("main"){capture::exclude_from_capture(&w);}Ok(())}).invoke_handler(tauri::generate_handler![list_devices,list_recordings,recording_status,start_recording,stop_recording,save_transcript,save_openai_key,openai_key_status,summarize_transcript,answer_question]).build(tauri::generate_context!()).expect("error while building tauri application").run(|app,event|{if let tauri::RunEvent::WindowEvent{event:tauri::WindowEvent::CloseRequested{..},..}=event{if let Some(state)=app.try_state::<AppState>(){if let Ok(mut g)=state.job.lock(){if let Some(mut j)=g.take(){j.stop_capture.store(true, Ordering::Relaxed); if let Some(handle)=j.capture_thread.take(){let _=handle.join();} if let Some(mut stdin)=j.stdin.take(){let _=stdin.write_all(b"q\n");} let _=j.child.wait();}}}}});}
