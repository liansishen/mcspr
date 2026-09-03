use crate::error::ApiResult;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropEntry {
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub key: Option<String>,
    pub value: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PropertiesFile {
    pub exists: bool,
    pub entries: Vec<PropEntry>,
}

pub fn read(dir: &Path) -> PropertiesFile {
    let path = dir.join("server.properties");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return PropertiesFile {
            exists: false,
            entries: vec![],
        };
    };
    let mut entries = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !pending.is_empty() {
                entries.push(PropEntry {
                    comment: std::mem::take(&mut pending),
                    key: None,
                    value: String::new(),
                });
            }
            entries.push(PropEntry {
                comment: String::new(),
                key: None,
                value: String::new(),
            });
        } else if t.starts_with('#') {
            pending.push_str(line);
            pending.push('\n');
        } else if let Some((k, v)) = line.split_once('=') {
            entries.push(PropEntry {
                comment: std::mem::take(&mut pending),
                key: Some(k.to_string()),
                value: v.to_string(),
            });
        } else {
            entries.push(PropEntry {
                comment: std::mem::take(&mut pending),
                key: None,
                value: line.to_string(),
            });
        }
    }
    if !pending.is_empty() {
        entries.push(PropEntry {
            comment: pending,
            key: None,
            value: String::new(),
        });
    }
    PropertiesFile { exists: true, entries }
}

pub async fn write(dir: &Path, entries: &[PropEntry]) -> ApiResult<()> {
    let mut out = String::new();
    for e in entries {
        if !e.comment.is_empty() {
            out.push_str(e.comment.trim_end_matches('\n'));
            out.push('\n');
        }
        match &e.key {
            Some(k) => out.push_str(&format!("{k}={}\n", e.value)),
            None => {
                out.push_str(&e.value);
                out.push('\n');
            }
        }
    }
    tokio::fs::write(dir.join("server.properties"), out).await?;
    Ok(())
}
