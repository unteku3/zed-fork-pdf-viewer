use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdfSendMode {
    Image,
    Text,
    #[default]
    ImageAndText,
}
impl PdfSendMode {
    pub fn includes_text(self) -> bool {
        self != Self::Image
    }
    pub fn includes_image(self) -> bool {
        self != Self::Text
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PdfViewerSettings {
    pub send: PdfSendMode,
    pub include_current_page: bool,
    pub pages_before: usize,
    pub pages_after: usize,
    pub auto_copy_region: bool,
    pub preamble_first: Option<PathBuf>,
    pub preamble_repeat: Option<PathBuf>,
}
impl PdfViewerSettings {
    pub fn from_value(v: serde_json::Value) -> anyhow::Result<Self> {
        let config_dir = v
            .get("_config_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let mut s: Self = serde_json::from_value(v)?;
        s.pages_before = s.pages_before.min(20);
        s.pages_after = s.pages_after.min(20);
        for p in [&mut s.preamble_first, &mut s.preamble_repeat]
            .into_iter()
            .flatten()
        {
            if let Ok(rest) = p.strip_prefix("~") {
                if let Some(home) = std::env::var_os("HOME") {
                    *p = PathBuf::from(home).join(rest);
                }
            }
            if p.is_relative() {
                if let Some(dir) = &config_dir {
                    *p = dir.join(&*p);
                }
            }
        }
        Ok(s)
    }
}
pub const MAX_PREAMBLE_BYTES: u64 = 64 * 1024;
pub fn read_preamble(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_PREAMBLE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() as u64 <= MAX_PREAMBLE_BYTES && !text.trim().is_empty())
        .then(|| text.trim().to_owned())
}
