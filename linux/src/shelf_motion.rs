use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

pub const OPEN_MS: u64 = 420;
pub const CLOSE_MS: u64 = 220;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RectBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub struct WindowBoundsTracker {
    pub requested: Option<RectBounds>,
    pub realized: Option<RectBounds>,
}

impl WindowBoundsTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn retains(&self, current: RectBounds, requested: RectBounds) -> bool {
        self.requested == Some(requested) && self.realized == Some(current)
    }

    pub fn record(&mut self, requested: RectBounds, realized: RectBounds, overlay: bool) {
        if overlay {
            self.requested = Some(requested);
            self.realized = Some(realized);
        } else {
            self.requested = None;
            self.realized = None;
        }
    }

    pub fn shown(&mut self, current: RectBounds) {
        if self.requested.is_some() {
            self.realized = Some(current);
        }
    }
}

struct PendingMotion {
    token: u64,
    phase: String,
    tx: Option<oneshot::Sender<bool>>,
}

pub struct MotionBridge {
    serial: u64,
    pending: Option<PendingMotion>,
}

impl MotionBridge {
    pub fn new() -> Self {
        Self {
            serial: 0,
            pending: None,
        }
    }

    pub fn cancel(&mut self) {
        if let Some(mut p) = self.pending.take() {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(false);
            }
        }
    }

    pub fn prepare_request(
        &mut self,
        phase: &str,
        details: &Value,
    ) -> (u64, Value, oneshot::Receiver<bool>, u64) {
        self.cancel();
        self.serial += 1;
        let token = self.serial;
        let (tx, rx) = oneshot::channel();
        self.pending = Some(PendingMotion {
            token,
            phase: phase.to_string(),
            tx: Some(tx),
        });
        let after = details.get("after").and_then(|v| v.as_u64()).unwrap_or(0);
        let base_ms = match phase {
            "open" => OPEN_MS,
            "close" => CLOSE_MS,
            _ => 0,
        };
        let timeout_ms = base_ms + after + 1000;
        let mut msg = details.as_object().cloned().unwrap_or_default();
        msg.insert("token".to_string(), json!(token));
        msg.insert("phase".to_string(), json!(phase));
        (token, Value::Object(msg), rx, timeout_ms)
    }

    pub fn acknowledge(&mut self, token: u64, phase: &str, completed: bool) -> bool {
        let matches = self
            .pending
            .as_ref()
            .map(|p| p.token == token && p.phase == phase)
            .unwrap_or(false);
        if !matches {
            return false;
        }
        if let Some(mut p) = self.pending.take() {
            if let Some(tx) = p.tx.take() {
                let _ = tx.send(completed);
            }
        }
        true
    }
}

impl Default for MotionBridge {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn request_motion<F>(
    bridge: &Arc<Mutex<MotionBridge>>,
    phase: &str,
    details: Value,
    send_fn: F,
) -> bool
where
    F: FnOnce(Value) -> Result<(), String>,
{
    let (token, payload, rx, timeout_ms) = {
        let mut guard = bridge.lock().unwrap();
        guard.prepare_request(phase, &details)
    };
    if send_fn(payload).is_err() {
        bridge.lock().unwrap().cancel();
        return false;
    }
    match tokio::time::timeout(Duration::from_millis(timeout_ms), rx).await {
        Ok(Ok(completed)) => completed,
        _ => {
            let mut guard = bridge.lock().unwrap();
            if guard.pending.as_ref().map(|p| p.token) == Some(token) {
                guard.pending = None;
            }
            true
        }
    }
}
