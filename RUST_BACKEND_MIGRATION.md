# Rust backend migration

The Go engine has been removed. Tauri commands, OpenAI transcription, key-file handling, transcript listing, summaries, and answer generation now run in Rust.

## Important limitations to verify
- The current recorder still invokes FFmpeg as an external process for audio capture. This removes Go, but does not yet remove the FFmpeg runtime dependency. Windows capture currently uses DirectShow devices (Stereo Mix / What U Hear), matching the supplied Go implementation.
- The non-Windows capture branch is a placeholder using AVFoundation device index `:0`; it requires testing and may need a proper ScreenCaptureKit backend for system audio on modern macOS.
- Build/test this archive on the target OS before relying on it. No cross-platform hardware capture validation is claimed.
- API key is saved to the user's config directory in `meet-capture/openai.key`; for production, prefer OS credential-store integration.

## Commands
```sh
npm install
npm run tauri dev
npm run build
cd src-tauri && cargo check
```

FFmpeg must be installed and on PATH for the current capture implementation. The `.env` file from the uploaded archive was intentionally excluded to avoid distributing secrets.
