// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

use super::*;
use std::sync::{Arc, Mutex};

/// A handle onto a `FakeEngine`'s received audio that survives the engine being
/// moved into a `Session`.
#[derive(Clone)]
pub struct AudioProbe {
    sent: Arc<Mutex<Vec<i16>>>,
}

impl AudioProbe {
    pub fn sent(&self) -> Vec<i16> {
        self.sent.lock().unwrap().clone()
    }
}

/// A handle onto every `SessionConfig` a `FakeEngine` was connected with,
/// surviving the engine being moved into a `Session`.
#[derive(Clone)]
pub struct ConfigProbe {
    configs: Arc<Mutex<Vec<SessionConfig>>>,
}

impl ConfigProbe {
    pub fn last(&self) -> Option<SessionConfig> {
        self.configs.lock().unwrap().last().cloned()
    }

    /// How many times this engine has been connected. One config is recorded
    /// per `connect()`, so this is the connect count -- what a test asserting
    /// that some path did *not* rotate the session actually needs, and the
    /// only such counter that survives the engine moving into a `Session`.
    pub fn count(&self) -> usize {
        self.configs.lock().unwrap().len()
    }
}

/// A handle onto a `FakeEngine`'s interrupt count that survives the engine
/// being moved into a `Session`.
#[derive(Clone)]
pub struct InterruptProbe {
    count: Arc<Mutex<usize>>,
}

impl InterruptProbe {
    pub fn count(&self) -> usize {
        *self.count.lock().unwrap()
    }
}

/// A handle onto a `FakeEngine`'s close count that survives the engine being
/// moved into a `Session`.
///
/// Exists because `disconnect()` returning the session to `Idle` is not the
/// thing that matters -- actually closing the connection is. A state
/// assertion alone would pass on a `disconnect` that forgot to close, which
/// is precisely the bug that leaves a billed session open.
#[derive(Clone)]
pub struct CloseProbe {
    count: Arc<Mutex<usize>>,
}

impl CloseProbe {
    pub fn count(&self) -> usize {
        *self.count.lock().unwrap()
    }
}

/// A handle onto a `FakeEngine`'s recorded tool results that survives the
/// engine being moved into a `Session`.
#[derive(Clone)]
pub struct ToolResultProbe {
    results: Arc<Mutex<Vec<(ToolCallId, ToolResult)>>>,
}

impl ToolResultProbe {
    pub fn results(&self) -> Vec<(ToolCallId, ToolResult)> {
        self.results.lock().unwrap().clone()
    }
}

/// Scripted engine for headless tests. Emits a fixed event list on connect and
/// records everything sent to it.
pub struct FakeEngine {
    script: Vec<EngineEvent>,
    sent_audio: Arc<Mutex<Vec<i16>>>,
    tool_results: Arc<Mutex<Vec<(ToolCallId, ToolResult)>>>,
    interrupts: Arc<Mutex<usize>>,
    closes: Arc<Mutex<usize>>,
    configs: Arc<Mutex<Vec<SessionConfig>>>,
    tx: Option<Sender<EngineEvent>>,
}

impl FakeEngine {
    pub fn new(script: Vec<EngineEvent>) -> Self {
        Self {
            script,
            sent_audio: Arc::new(Mutex::new(Vec::new())),
            tool_results: Arc::new(Mutex::new(Vec::new())),
            interrupts: Arc::new(Mutex::new(0)),
            closes: Arc::new(Mutex::new(0)),
            configs: Arc::new(Mutex::new(Vec::new())),
            tx: None,
        }
    }
    pub fn sent_audio(&self) -> Vec<i16> {
        self.sent_audio.lock().unwrap().clone()
    }
    pub fn tool_results(&self) -> Vec<(ToolCallId, ToolResult)> {
        self.tool_results.lock().unwrap().clone()
    }
    pub fn interrupt_count(&self) -> usize {
        *self.interrupts.lock().unwrap()
    }

    /// Take an observation handle before moving this engine into a `Session`.
    /// Shares the underlying buffer, so the test can still see what the session
    /// sent after ownership has moved.
    pub fn audio_probe(&self) -> AudioProbe {
        AudioProbe {
            sent: Arc::clone(&self.sent_audio),
        }
    }

    /// Take an observation handle onto the interrupt count before moving this
    /// engine into a `Session`.
    pub fn interrupt_probe(&self) -> InterruptProbe {
        InterruptProbe {
            count: Arc::clone(&self.interrupts),
        }
    }

    /// Take an observation handle onto the close count before moving this
    /// engine into a `Session`.
    pub fn close_probe(&self) -> CloseProbe {
        CloseProbe {
            count: Arc::clone(&self.closes),
        }
    }

    /// Take an observation handle onto recorded tool results before moving
    /// this engine into a `Session`.
    pub fn tool_result_probe(&self) -> ToolResultProbe {
        ToolResultProbe {
            results: Arc::clone(&self.tool_results),
        }
    }

    /// Take an observation handle onto every `SessionConfig` this engine is
    /// connected with, before moving it into a `Session`.
    pub fn config_probe(&self) -> ConfigProbe {
        ConfigProbe {
            configs: Arc::clone(&self.configs),
        }
    }

    /// Push an extra event after connect, so tests can drive mid-session behaviour.
    pub async fn emit(&self, ev: EngineEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev).await;
        }
    }
}

#[async_trait]
impl S2sEngine for FakeEngine {
    fn id(&self) -> EngineId {
        EngineId::OpenAi
    }
    fn input_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(16_000)
    }
    fn output_format(&self) -> AudioFormat {
        AudioFormat::mono_pcm16(24_000)
    }

    async fn connect(
        &mut self,
        cfg: &SessionConfig,
        tx: Sender<EngineEvent>,
    ) -> Result<(), EngineError> {
        self.configs.lock().unwrap().push(cfg.clone());
        for ev in self.script.clone() {
            let _ = tx.send(ev).await;
        }
        self.tx = Some(tx);
        Ok(())
    }
    async fn send_audio(&mut self, frame: &[i16]) -> Result<(), EngineError> {
        self.sent_audio.lock().unwrap().extend_from_slice(frame);
        Ok(())
    }
    async fn send_tool_result(
        &mut self,
        id: ToolCallId,
        result: ToolResult,
    ) -> Result<(), EngineError> {
        self.tool_results.lock().unwrap().push((id, result));
        Ok(())
    }
    async fn interrupt(&mut self) -> Result<(), EngineError> {
        *self.interrupts.lock().unwrap() += 1;
        Ok(())
    }
    async fn close(&mut self) -> Result<(), EngineError> {
        *self.closes.lock().unwrap() += 1;
        Ok(())
    }
}
