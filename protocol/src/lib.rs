//! Fork-specific graphic extension protocol. stdout is exclusively framed IPC.
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
pub const API_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
pub const CACHE_BUDGET: usize = 128 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub platform: String,
    pub executable: String,
    pub extensions: Vec<String>,
    pub settings_key: String,
    #[serde(default)]
    pub settings: serde_json::Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub view: u64,
    pub command: Command,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Command {
    Hello {
        api_version: u32,
    },
    Open {
        path: String,
        settings: serde_json::Value,
        state: serde_json::Value,
    },
    Update {
        width: f32,
        height: f32,
        scale: f32,
        event: Event,
        settings: serde_json::Value,
    },
    Context {
        selection: u64,
        images: bool,
        first: bool,
    },
    Close,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    Refresh,
    Resize,
    Action(String),
    Scroll {
        x: f32,
        y: f32,
    },
    Pointer {
        kind: String,
        x: f32,
        y: f32,
        alt: bool,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub error: Option<String>,
    pub scene: Option<Scene>,
    pub context: Option<ContextPayload>,
    pub clipboard: Option<Clipboard>,
    /// Number of raw binary frames following the JSON response.
    pub blobs: usize,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scene {
    pub nodes: Vec<Node>,
    pub selection: Option<Selection>,
    pub pending: bool,
    #[serde(default)]
    pub state: serde_json::Value,
    #[serde(default)]
    pub keys: std::collections::BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub bounds: [f32; 4],
    pub kind: NodeKind,
    #[serde(default)]
    pub tooltip: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum NodeKind {
    Text(String),
    Button {
        label: String,
        action: String,
    },
    Image {
        key: String,
        blob: Option<usize>,
        width: u32,
        height: u32,
    },
    Rect {
        color: u32,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selection {
    pub token: u64,
    pub path: String,
    pub text: String,
    pub first: String,
    pub repeat: String,
    pub first_no_images: String,
    pub repeat_no_images: String,
    pub image_names: Vec<String>,
    pub terminal_first: String,
    pub terminal_repeat: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextPayload {
    pub text: String,
    pub image_names: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Clipboard {
    Text(String),
    Png { blob: usize },
}
pub fn write_frame(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut header = [0; 4];
    r.read_exact(&mut header)?;
    let n = u32::from_le_bytes(header) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut result = vec![0; n];
    r.read_exact(&mut result)?;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_roundtrip_and_bounds() {
        let mut wire = vec![];
        write_frame(&mut wire, b"hello").unwrap();
        assert_eq!(read_frame(&mut &wire[..]).unwrap(), b"hello");
        assert!(read_frame(&mut &((MAX_FRAME as u32) + 1).to_le_bytes()[..]).is_err());
        assert!(read_frame(&mut &wire[..6]).is_err());
    }
}
