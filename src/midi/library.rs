use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::Deserialize;
use uuid::Uuid;

static ASSETS_DIR: Lazy<PathBuf> = Lazy::new(|| PathBuf::from("assets/midi"));
static MANIFEST_PATH: Lazy<PathBuf> = Lazy::new(|| PathBuf::from("assets/midi_manifest.json"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiOrigin {
    Asset,
    Local,
}

#[derive(Debug, Clone)]
pub struct MidiEntry {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub origin: MidiOrigin,
    pub library_path: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone)]
pub struct MidiLibrary {
    entries: Vec<MidiEntry>,
    index_by_id: HashMap<Uuid, usize>,
    index_by_path: HashMap<PathBuf, Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest(Vec<String>);

impl MidiLibrary {
    pub fn load_with_assets() -> Result<Self> {
        let mut library = MidiLibrary::default();
        if MANIFEST_PATH.exists() {
            let manifest = fs::read_to_string(&*MANIFEST_PATH)
                .with_context(|| format!("failed to read {}", MANIFEST_PATH.display()))?;
            let entries: Manifest =
                serde_json::from_str(&manifest).context("failed to parse MIDI manifest")?;
            for item in entries.0 {
                let candidate = ASSETS_DIR.join(&item);
                if candidate.exists() {
                    let mut parts: Vec<String> = item
                        .split('/')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let _ = library.insert_entry(
                        candidate,
                        MidiOrigin::Asset,
                        if parts.len() > 1 {
                            parts.pop();
                            Some(parts)
                        } else {
                            None
                        },
                    );
                } else {
                    log::warn!("skipping missing asset entry {}", candidate.display());
                }
            }
        } else {
            log::warn!(
                "MIDI manifest not found at {}, starting with empty asset library",
                MANIFEST_PATH.display()
            );
        }
        Ok(library)
    }

    pub fn entries(&self) -> &[MidiEntry] {
        &self.entries
    }

    pub fn get(&self, id: &Uuid) -> Option<&MidiEntry> {
        self.index_by_id
            .get(id)
            .and_then(|index| self.entries.get(*index))
    }

    pub fn add_local_file<P: AsRef<Path>>(&mut self, path: P) -> Result<&MidiEntry> {
        let path = normalize_path(path.as_ref());
        let entry_id = if let Some(existing) = self.index_by_path.get(&path) {
            *existing
        } else {
            self.insert_entry(path, MidiOrigin::Local, None)
        };
        self.index_by_id
            .get(&entry_id)
            .and_then(|idx| self.entries.get(*idx))
            .context("failed to retrieve newly added MIDI entry")
    }

    fn insert_entry<P: Into<PathBuf>>(
        &mut self,
        path: P,
        origin: MidiOrigin,
        library_path: Option<Vec<String>>,
    ) -> Uuid {
        let raw_path: PathBuf = path.into();
        let path = normalize_path(&raw_path);
        let id = Uuid::new_v4();
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| path.display().to_string());
        let entry = MidiEntry {
            id,
            name,
            path: path.clone(),
            origin,
            library_path,
        };
        self.index_by_id.insert(id, self.entries.len());
        self.index_by_path.insert(path, id);
        self.entries.push(entry);
        id
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(canon) => canon,
        Err(_) => path.to_path_buf(),
    }
}
