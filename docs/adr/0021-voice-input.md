# ADR-0021: Voice input: local speech models in a process that is ended when idle

- **Status:** Accepted. Implemented 2026-11-19.
- **Date:** 2026-11-19

## Context

The product owner asked for dictation like T3 Code's "Voice": speak into the composer, get text to edit or send. Two further asks: offer a transcript cleanup step with small local models as well as cheap models from the agents' subscriptions, and never keep a model loaded (hogging memory) when it isn't being used.

T3 Code (read 2026-11-19) transcribes on its server with transcribe.cpp in a child process, from a catalog of GGUF speech models published by handy-computer (Parakeet, Nemotron, Canary, Cohere, Whisper, Qwen3-ASR, Moonshine…), each pinned to a commit and checked by SHA-256. It unloads a model after an idle timeout (`never`, `immediately`, 2, 5, 10, 15 minutes, 1 hour; 15 minutes by default) by ending that child. Cleanup there goes to the environment's text generation model (an agent, or an API "text generator").

## Decision

1. **Where audio goes.** The client records (desktop: cpal; browser: `getUserMedia` + Web Audio) and resamples to 16 kHz mono 16-bit. With a model that transcribes live (Parakeet English, Nemotron 3.5: `streaming` in the catalog), it sends the recording in pieces of at least a quarter second as it comes, one at a time (`VoiceLiveStart`, `VoiceLiveFeed`, `VoiceLiveFinish`); each answer carries the text so far, committed and tentative, which the composer shows above its toolbar. The stream lives in the engine process (`transcribe_cpp::Stream` borrows the session, so the engine runs it in a loop of its own until finish or cancel). Other models get the whole recording at the end (`VoiceTranscribe`, at most 5 minutes: 9.6 MB, 12.8 MB base64, inside a web session's 16 MiB frame), and so does a live one that failed. Nothing is kept afterwards. A window on another computer (SSH, browser) dictates with that computer's models.
2. **Engine.** transcribe.cpp through the `transcribe-cpp` crate (Metal on Apple silicon), the same engine and models as T3 Code. The catalog (`crates/loft-voice/src/catalog.rs`) is 12 of its models, Parakeet English first. Downloads go to `<home>/models/<id>/` through a `.part` file (resumed with a range request), are checked by SHA-256 and only then renamed into place. A copy already on the computer (Hugging Face cache, the other Agentloft home) is checked and hard-linked instead of downloaded again.
3. **Unloading.** Each model runs in `agentloftd voice-engine`, a child process holding one model, spoken to with JSON lines on stdin/stdout. One process for the speech model and one for a local cleanup model. After each use a timer starts (`[voice] unload_after`, default **5 minutes**, same choices as T3 Code); if nothing used the engine before it fires, the process is killed. Ending the process is the only way to be sure every byte comes back: GPU buffers, allocator caches (macOS keeps freed blocks of hundreds of MB inside a process) and the model's mapping. Switching models also starts a fresh process. The engine exits when its stdin closes, so it never outlives the daemon.
4. **Cleanup** (`[voice] cleanup`, off by default) sends text, never audio:
   - `local:<model>`: a small model on this computer, run by the engine process on candle (`crates/loft-llm`: Qwen3 0.6B/1.7B/4B, Gemma 3 1B/4B as GGUF, Metal on a Mac). Free and private; unloaded like the speech model. Gemma 3 4B is recommended: the only one that reliably applies spoken corrections ("42, sorry, 24" → 24). llama.cpp was not used: it vendors its own ggml, as transcribe.cpp does, and two static copies can't share one binary.
   - `<agent>:<model>`: the agent's CLI on the user's plan through `Core::ask_once` (no tools, no session), e.g. `claude:haiku` (about 2 s).
   The result is checked (`cleanup::tidy`: an empty or runaway answer falls back to the transcript as heard). The client offers Skip while it runs, and inserts the raw text if it fails.
5. **Composer.** A microphone button and ⇧⌘D (`dictate` in Shortcuts). The text goes where the cursor or selection was when recording began, unless the draft changed meanwhile (then at the cursor). Esc or ✕ throws the recording away.

## Consequences

- `agentloftd` links transcribe.cpp (built with CMake; about 25 s the first time) and candle. The app does not.
- The website's frame (`/m/<id>`) and the daemon's web pages now allow the microphone for the framed computer's origin (`Permissions-Policy`), and the iframe has `allow="microphone"`. Browsers allow `getUserMedia` only in a secure context: plain HTTP over the LAN can't dictate.
- While a live transcription runs, its model stays loaded at least a minute after the last piece, whatever `unload_after` says (a window that went away mid-recording is ended then).
- Not done: cloud speech APIs (OpenAI, xAI), a per-project dictionary, silence filtering (Silero VAD). Long recordings are cut at their quietest moments when a model has a length limit.
