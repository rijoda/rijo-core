//! watcher — hot-reload of config.yaml and skills directory
//!
//! Watches:
//!   - ~/.rijo/config.yaml  → UiEvent::ConfigReloaded(Config)
//!   - ~/.rijo/skills/**    → UiEvent::SkillsReloaded
//!
//! Debounce 800 ms (absorbs editor rename/save bursts and file-writer churn).
//! Reload I/O runs inline on this worker thread; results go into the TUI
//! event channel.

//! watcher — hot-reload of config.yaml and skills directory
//!
//! Watches:
//!   - ~/.rijo/config.yaml  → UiEvent::ConfigReloaded(Config)
//!   - ~/.rijo/skills/**    → UiEvent::SkillsReloaded
//!
//! Debounce 800 ms (absorbs editor rename/save bursts and file-writer churn).
//! Reload I/O runs inline on this worker thread; results go into the TUI
//! event channel.
//!
//! Skills-reload fires only when content actually changed (mtime-summary
//! comparison), not on every FS event.

use crate::ui_events::UiEvent;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

const DEBOUNCE: Duration = Duration::from_millis(800);

/// Snapshot of (filename → mtime) for a directory, used to detect real changes.
#[derive(Clone, Default)]
struct MtimeSnapshot(HashMap<String, u128>);

impl MtimeSnapshot {
    /// Collect all files under `dir` (recursive) with their modified timestamp.
    fn capture(dir: &Path) -> Self {
        let mut map = HashMap::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // recurse
                    let sub = Self::capture(&path);
                    map.extend(sub.0);
                } else if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let dur = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        if let Some(name) = path.to_str() {
                            map.insert(name.to_string(), dur.as_millis());
                        }
                    }
                }
            }
        }
        Self(map)
    }

    /// True iff this snapshot differs from `other` (any file added/removed/modified).
    fn differs_from(&self, other: &Self) -> bool {
        if self.0.len() != other.0.len() {
            return true;
        }
        for (k, v) in &self.0 {
            if other.0.get(k) != Some(v) {
                return true;
            }
        }
        false
    }
}

#[derive(Default)]
struct Pendings {
    config: Option<Instant>,
    skills: Option<Instant>,
    staging: Option<Instant>,
}

impl Pendings {
    fn ready_drain(&mut self) -> (bool, bool, bool) {
        let now = Instant::now();
        let mut fire_config = false;
        let mut fire_skills = false;
        let mut fire_staging = false;
        if let Some(t) = self.config {
            if now.duration_since(t) >= DEBOUNCE {
                fire_config = true;
                self.config = None;
            }
        }
        if let Some(t) = self.skills {
            if now.duration_since(t) >= DEBOUNCE {
                fire_skills = true;
                self.skills = None;
            }
        }
        if let Some(t) = self.staging {
            if now.duration_since(t) >= DEBOUNCE {
                fire_staging = true;
                self.staging = None;
            }
        }
        (fire_config, fire_skills, fire_staging)
    }
}

pub fn spawn(tx: UnboundedSender<UiEvent>) -> Result<(), String> {
    let (raw_tx, raw_rx) = channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            let _ = raw_tx.send(res);
        },
        notify::Config::default(),
    )
    .map_err(|e| e.to_string())?;

    let rijo_home = crate::paths::rijo_home();
    let config_path = rijo_home.join("config.yaml");
    let skills_dir = rijo_home.join("skills");
    let staging_dir = crate::selfupd::staging_dir();
    let _ = std::fs::create_dir_all(&staging_dir);

    if config_path.exists() {
        watcher
            .watch(&config_path, RecursiveMode::NonRecursive)
            .map_err(|e| format!("watch config: {e}"))?;
    }
    let _ = std::fs::create_dir_all(&skills_dir);
    watcher
        .watch(&skills_dir, RecursiveMode::Recursive)
        .map_err(|e| format!("watch skills: {e}"))?;
    watcher
        .watch(&staging_dir, RecursiveMode::NonRecursive)
        .map_err(|e| format!("watch staging: {e}"))?;

    let mut pend = Pendings::default();

    // snapshot skills dir content to avoid re-firing on spurious FS events
    let _ = std::fs::create_dir_all(&skills_dir);
    let mut skills_last_snapshot = MtimeSnapshot::capture(&skills_dir);

    // block on the first event (cheap); a closed channel ends the loop
    while let Ok(res) = raw_rx.recv() {
        // classify event; update pendings
        if let Ok(evt) = res {
            let touches_config = evt.paths.iter().any(|p| p.ends_with("config.yaml"));
            let touches_skills = evt.paths.iter().any(|p| p.starts_with(&skills_dir));
            let touches_staging = evt.paths.iter().any(|p| p.starts_with(&staging_dir));
            let now = Instant::now();
            if touches_config {
                pend.config = Some(now);
            }
            if touches_skills {
                pend.skills = Some(now);
            }
            if touches_staging {
                pend.staging = Some(now);
            }
        }

        // inner drain: while any pending, keep consuming (non-blocking drain)
        loop {
            // collect everything ready *after* the quiet period elapses
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(Ok(evt2)) => {
                    let now = Instant::now();
                    if evt2.paths.iter().any(|p| p.ends_with("config.yaml")) {
                        pend.config = Some(now);
                    }
                    if evt2.paths.iter().any(|p| p.starts_with(&skills_dir)) {
                        pend.skills = Some(now);
                    }
                    if evt2.paths.iter().any(|p| p.starts_with(&staging_dir)) {
                        pend.staging = Some(now);
                    }
                    continue; // keep absorbing within debounce window
                }
                Ok(Err(_e)) => continue, // fs error; ignore
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        // now the quiet period is over: fire combined
        let (fc, fs, fst) = pend.ready_drain();
        if fc {
            let cfg = crate::config::Config::load(None);
            if tx.send(UiEvent::ConfigReloaded(cfg)).is_err() {
                return Ok(());
            }
        }
        if fs {
            let new_snap = MtimeSnapshot::capture(&skills_dir);
            if new_snap.differs_from(&skills_last_snapshot) {
                skills_last_snapshot = new_snap;
                if tx.send(UiEvent::SkillsReloaded).is_err() {
                    return Ok(());
                }
            }
        }
        if fst && tx.send(UiEvent::StagedBinary).is_err() {
            return Ok(());
        }
        // re-resume watch for the config file if it was renamed away (atomic writers)
        let exists = config_path.exists();
        if exists {
            // re-arm, since some editors replace the file (new inode)
            let _ = watcher.watch(&config_path, RecursiveMode::NonRecursive);
        }
        let _ = std::fs::create_dir_all(&skills_dir);
        let _ = watcher.watch(&skills_dir, RecursiveMode::Recursive);
    }

    Ok(())
}
