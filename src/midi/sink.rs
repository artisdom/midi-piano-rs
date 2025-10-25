use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiTransport {
    Usb,
    Bluetooth,
}

#[derive(Debug, Clone)]
pub struct MidiSinkInfo {
    pub id: uuid::Uuid,
    pub name: String,
    pub transport: MidiTransport,
}

impl MidiSinkInfo {
    pub fn with_id(id: uuid::Uuid, name: impl Into<String>, transport: MidiTransport) -> Self {
        Self {
            id,
            name: name.into(),
            transport,
        }
    }
}

#[async_trait]
pub trait MidiSink: Send + Sync {
    async fn send(&self, data: &[u8]) -> Result<()>;

    async fn send_batch(&self, _timestamp_ms: u16, messages: &[Vec<u8>]) -> Result<()> {
        for message in messages {
            self.send(message).await?;
        }
        Ok(())
    }
}

pub type SharedMidiSink = Arc<dyn MidiSink>;
