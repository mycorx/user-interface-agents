//! Throwaway spike: learn Nova Sonic's real bidirectional-stream wire shape.
//!
//! Not shipped code. It opens a real stream against `amazon.nova-2-sonic-v1:0`,
//! drives one cross-modal text turn (so no microphone is needed), and dumps every
//! response event verbatim. A second mode probes which input sample rates the
//! service actually accepts, rather than trusting the documented enum.
//!
//! Usage:
//!   cargo run                     # full text turn + tool round trip
//!   cargo run -- rate 44100       # audio contentStart at a given rate; print accept/reject

use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::{BidirectionalInputPayloadPart, InvokeModelWithBidirectionalStreamInput};
use base64::Engine as _;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

type EventTx = mpsc::Sender<Result<InvokeModelWithBidirectionalStreamInput, aws_sdk_bedrockruntime::types::error::InvokeModelWithBidirectionalStreamInputError>>;

const MODEL_ID: &str = "amazon.nova-2-sonic-v1:0";
const REGION: &str = "ap-northeast-1";
const PROMPT: &str = "probe-prompt-1";

async fn send(tx: &EventTx, label: &str, json: serde_json::Value) {
    let text = json.to_string();
    println!(">>> {label}: {text}");
    let part = BidirectionalInputPayloadPart::builder()
        .bytes(Blob::new(text.into_bytes()))
        .build();
    if let Err(e) = tx
        .send(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(part)))
        .await
    {
        println!("!!! could not queue {label}: {e}");
    }
}

/// Truncate the huge base64 audio payloads so the log stays readable.
fn summarize(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(mut v) => {
            for key in ["audioOutput"] {
                if let Some(content) = v
                    .get_mut("event")
                    .and_then(|e| e.get_mut(key))
                    .and_then(|a| a.get_mut("content"))
                    && let Some(s) = content.as_str()
                {
                    let n = s.len();
                    *content = serde_json::Value::String(format!("<{n} base64 chars elided>"));
                }
            }
            v.to_string()
        }
        Err(_) => raw.to_string(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    // `audio <raw-pcm-path> <sampleRateHertz>` streams real speech; no arg = text turn.
    let audio_probe: Option<(String, i32)> = if args.get(1).map(String::as_str) == Some("audio") {
        Some((args[2].clone(), args[3].parse()?))
    } else {
        None
    };
    let rate_probe = audio_probe.as_ref().map(|(_, r)| *r);

    let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(REGION)
        .load()
        .await;
    let client = Client::new(&cfg);

    let (tx, rx) = mpsc::channel(64);

    // GOTCHA (paid for once): the setup events must already be queued before
    // `send()` is awaited. Nova sends no response headers until it has received
    // `sessionStart`, and `send()` does not return until those headers arrive —
    // queueing afterwards deadlocks with no error and no output.
    // --- session setup -----------------------------------------------------
    send(
        &tx,
        "sessionStart",
        serde_json::json!({"event": {"sessionStart": {
            "inferenceConfiguration": {"maxTokens": 512, "topP": 0.9, "temperature": 0.7},
            "turnDetectionConfiguration": {"endpointingSensitivity": "MEDIUM"}
        }}}),
    )
    .await;

    send(
        &tx,
        "promptStart",
        serde_json::json!({"event": {"promptStart": {
            "promptName": PROMPT,
            "textOutputConfiguration": {"mediaType": "text/plain"},
            "audioOutputConfiguration": {
                "mediaType": "audio/lpcm",
                "sampleRateHertz": 24000,
                "sampleSizeBits": 16,
                "channelCount": 1,
                "voiceId": "matthew",
                "encoding": "base64",
                "audioType": "SPEECH"
            },
            "toolUseOutputConfiguration": {"mediaType": "application/json"},
            "toolConfiguration": {"tools": [{"toolSpec": {
                "name": "get_time",
                "description": "Get the current local time for a city.",
                "inputSchema": {"json": "{\"type\":\"object\",\"properties\":{\"city\":{\"type\":\"string\"}},\"required\":[\"city\"]}"}
            }}]}
        }}}),
    )
    .await;

    // System prompt (TEXT / SYSTEM).
    send(&tx, "contentStart(SYSTEM)", serde_json::json!({"event": {"contentStart": {
        "promptName": PROMPT, "contentName": "sys-1", "type": "TEXT", "interactive": false,
        "role": "SYSTEM", "textInputConfiguration": {"mediaType": "text/plain"}
    }}})).await;
    send(&tx, "textInput(SYSTEM)", serde_json::json!({"event": {"textInput": {
        "promptName": PROMPT, "contentName": "sys-1",
        "content": "You are a terse voice assistant. When asked the time in a city, call the get_time tool."
    }}})).await;
    send(&tx, "contentEnd(SYSTEM)", serde_json::json!({"event": {"contentEnd": {
        "promptName": PROMPT, "contentName": "sys-1"
    }}})).await;

    if let Some((path, rate)) = audio_probe.clone() {
        // Real speech in, at a declared rate. A wrong rate is how we learn which
        // values the service actually accepts, rather than trusting the doc enum.
        send(&tx, "contentStart(AUDIO)", serde_json::json!({"event": {"contentStart": {
            "promptName": PROMPT, "contentName": "aud-1", "type": "AUDIO", "interactive": true,
            "role": "USER",
            "audioInputConfiguration": {
                "mediaType": "audio/lpcm", "sampleRateHertz": rate, "sampleSizeBits": 16,
                "channelCount": 1, "audioType": "SPEECH", "encoding": "base64"
            }
        }}})).await;

        // Stream the PCM in ~32 ms frames at the declared rate, paced like a mic.
        let pcm = std::fs::read(&path)?;
        let frame_bytes = (rate as usize / 1000 * 32) * 2;
        let frames = pcm.len().div_ceil(frame_bytes);
        println!("--- streaming {} bytes of PCM as {frames} frames of {frame_bytes} bytes", pcm.len());
        let tx_audio = tx.clone();
        tokio::spawn(async move {
            for chunk in pcm.chunks(frame_bytes) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
                let ev = serde_json::json!({"event": {"audioInput": {
                    "promptName": PROMPT, "contentName": "aud-1", "content": b64
                }}});
                let part = BidirectionalInputPayloadPart::builder()
                    .bytes(Blob::new(ev.to_string().into_bytes()))
                    .build();
                if tx_audio
                    .send(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(part)))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(32)).await;
            }
            // Trailing silence gives the endpointer a pause to detect.
            for _ in 0..15 {
                let b64 = base64::engine::general_purpose::STANDARD.encode(vec![0u8; frame_bytes]);
                let ev = serde_json::json!({"event": {"audioInput": {
                    "promptName": PROMPT, "contentName": "aud-1", "content": b64
                }}});
                let part = BidirectionalInputPayloadPart::builder()
                    .bytes(Blob::new(ev.to_string().into_bytes()))
                    .build();
                if tx_audio
                    .send(Ok(InvokeModelWithBidirectionalStreamInput::Chunk(part)))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(32)).await;
            }
            println!("--- audio streamed");
        });
    } else {
        // Cross-modal text turn: forces a completion with no microphone involved.
        send(&tx, "contentStart(USER)", serde_json::json!({"event": {"contentStart": {
            "promptName": PROMPT, "contentName": "usr-1", "type": "TEXT", "interactive": true,
            "role": "USER", "textInputConfiguration": {"mediaType": "text/plain"}
        }}})).await;
        send(&tx, "textInput(USER)", serde_json::json!({"event": {"textInput": {
            "promptName": PROMPT, "contentName": "usr-1", "content": "What time is it in Sydney?"
        }}})).await;
        send(&tx, "contentEnd(USER)", serde_json::json!({"event": {"contentEnd": {
            "promptName": PROMPT, "contentName": "usr-1"
        }}})).await;
    }

    let body = ReceiverStream::new(rx).into();
    let mut out = match client
        .invoke_model_with_bidirectional_stream()
        .model_id(MODEL_ID)
        .body(body)
        .send()
        .await
    {
        Ok(out) => {
            println!("stream opened against {MODEL_ID} in {REGION}");
            out
        }
        Err(e) => {
            println!("ERROR opening stream: {e:?}");
            return Ok(());
        }
    };

    // --- read every response event ----------------------------------------
    let mut answered_tool = false;
    let mut closed_prompt = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(90);

    loop {
        let ev = tokio::select! {
            r = out.body.recv() => r,
            _ = tokio::time::sleep_until(deadline) => { println!("--- deadline reached"); break; }
        };
        match ev {
            Ok(Some(ev)) => {
                let raw = match &ev {
                    aws_sdk_bedrockruntime::types::InvokeModelWithBidirectionalStreamOutput::Chunk(part) => part
                        .bytes()
                        .map(|b| String::from_utf8_lossy(b.as_ref()).to_string())
                        .unwrap_or_default(),
                    other => format!("{other:?}"),
                };
                println!("EVENT: {}", summarize(&raw));

                // Complete the tool round trip so we see the full cycle.
                if !answered_tool
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw)
                    && let Some(tool) = v.get("event").and_then(|e| e.get("toolUse"))
                {
                    answered_tool = true;
                    let id = tool.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("");
                    send(&tx, "contentStart(TOOL)", serde_json::json!({"event": {"contentStart": {
                        "promptName": PROMPT, "contentName": "tool-1", "interactive": false,
                        "type": "TOOL", "role": "TOOL",
                        "toolResultInputConfiguration": {
                            "toolUseId": id, "type": "TEXT",
                            "textInputConfiguration": {"mediaType": "text/plain"}
                        }
                    }}})).await;
                    send(&tx, "toolResult", serde_json::json!({"event": {"toolResult": {
                        "promptName": PROMPT, "contentName": "tool-1",
                        "content": "{\"time\":\"00:54\",\"city\":\"Sydney\"}"
                    }}})).await;
                    send(&tx, "contentEnd(TOOL)", serde_json::json!({"event": {"contentEnd": {
                        "promptName": PROMPT, "contentName": "tool-1"
                    }}})).await;
                }

                // Does closing the prompt release `completionEnd`? Send promptEnd as
                // soon as the assistant's audio turn ends, then keep reading.
                if !closed_prompt
                    && raw.contains("\"stopReason\":\"END_TURN\"")
                    && raw.contains("\"type\":\"AUDIO\"")
                {
                    closed_prompt = true;
                    send(&tx, "contentEnd(AUDIO-in)", serde_json::json!({"event": {"contentEnd": {
                        "promptName": PROMPT, "contentName": "aud-1"
                    }}})).await;
                    send(&tx, "promptEnd", serde_json::json!({"event": {"promptEnd": {"promptName": PROMPT}}})).await;
                }

                if raw.contains("\"completionEnd\"") {
                    println!("--- completionEnd seen; closing");
                    break;
                }
            }
            Ok(None) => {
                println!("--- server closed the stream");
                break;
            }
            Err(e) => {
                println!("STREAM ERROR: {e:?}");
                break;
            }
        }
    }

    if !closed_prompt {
        send(&tx, "promptEnd", serde_json::json!({"event": {"promptEnd": {"promptName": PROMPT}}})).await;
    }
    send(&tx, "sessionEnd", serde_json::json!({"event": {"sessionEnd": {}}})).await;
    drop(tx);
    Ok(())
}
