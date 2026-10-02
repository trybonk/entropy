use super::*;
use crate::key_stats::{
    is_held_keycode, split_halves, KeyPos, KeyStatsStore, StatsMonth, TransitionRecorder,
};
use layout_indicator::LayerTrackerPress;

const KEY_STATS_AUTOSAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// How often the matrix poll rate is written to the diagnostics log.
const KEY_STATS_RATE_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Why presses are currently not counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyStatsPauseReason {
    Disabled,
    NoKeyboard,
    /// The matrix can only be read from Vial firmware.
    Unsupported,
    Locked,
    MatrixTester,
    TypingTrainer,
}

/// Collected statistics of the connected keyboard and the state needed to
/// turn matrix polls into counters.
#[derive(Default)]
pub(crate) struct KeyStatsRuntime {
    pub(crate) store: KeyStatsStore,
    /// Storage folder of the keyboard whose statistics are loaded.
    pub(crate) keyboard_key: Option<String>,
    recorder: TransitionRecorder,
    /// Keyboard half per matrix position, rebuilt when the layout changes.
    matrix_halves: Vec<u8>,
    matrix_halves_source: Option<(String, usize, usize)>,
    last_save: Option<std::time::Instant>,
    was_collecting: bool,
    /// Matrix samples and presses since `rate_window_start`, for the
    /// diagnostics log: a too slow poll (e.g. in the tray) misses presses.
    rate_window_start: Option<std::time::Instant>,
    rate_polls: u32,
    rate_presses: u32,
    rate_hidden_polls: u32,
}

fn key_stats_root_dir() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("entropy")
        .join("key_stats")
}

fn load_key_stats(dir: &std::path::Path) -> KeyStatsStore {
    let mut store = KeyStatsStore::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
        Err(error) => {
            log::warn!("load_key_stats {}: {error}", dir.display());
            return store;
        }
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        let is_month_file = path.extension().is_some_and(|ext| ext == "json")
            && path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(StatsMonth::parse_file_stem)
                .is_some();
        if !is_month_file {
            continue;
        }
        let loaded = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|data| store.load_month(&data));
        if let Err(error) = loaded {
            // Keep the unreadable file aside so the next save cannot replace
            // a month of history with only the newest presses.
            log::warn!("load_key_stats {}: {error}", path.display());
            let _ = std::fs::rename(&path, path.with_extension("json.bad"));
        }
    }
    store
}

#[cfg(not(target_arch = "wasm32"))]
fn write_key_stats_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::Builder::new()
        .prefix(".key-stats-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn write_key_stats_file(_path: &std::path::Path, _contents: &[u8]) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

fn save_key_stats(dir: &std::path::Path, store: &mut KeyStatsStore) {
    for month in store.dirty_months() {
        let path = dir.join(format!("{}.json", month.file_stem()));
        let saved = match store.month_json(month) {
            Some(Ok(json)) => {
                write_key_stats_file(&path, json.as_bytes()).map_err(|error| error.to_string())
            }
            Some(Err(error)) => Err(error),
            None => match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error.to_string())
                }
                _ => Ok(()),
            },
        };
        match saved {
            Ok(()) => store.mark_saved(month),
            Err(error) => log::warn!("save_key_stats {}: {error}", path.display()),
        }
    }
}

fn matrix_halves(layout: &KeyboardLayout) -> Vec<u8> {
    let key_halves = split_halves(&layout.keys);
    let mut halves = vec![0; layout.rows * layout.cols];
    for (key, half) in layout.keys.iter().zip(key_halves) {
        if let Some(slot) = halves.get_mut(key.row as usize * layout.cols + key.col as usize) {
            *slot = half;
        }
    }
    halves
}

impl EntropyApp {
    /// Storage folder name of the connected keyboard: its Vial keyboard id,
    /// or the layout name when the firmware does not report one.
    fn key_stats_keyboard_key(&self) -> Option<String> {
        if let Some(id) = self.current_keyboard_id.filter(|id| *id != 0) {
            return Some(format!("{id:016x}"));
        }
        self.layout
            .as_ref()
            .map(|layout| device_id_slug(&layout.name))
            .filter(|slug| !slug.trim_matches('_').is_empty())
    }

    pub(crate) fn key_stats_pause_reason(&self) -> Option<KeyStatsPauseReason> {
        if !self.app_settings.key_heatmap.enabled {
            return Some(KeyStatsPauseReason::Disabled);
        }
        if self.layout.is_none() || self.key_stats_keyboard_key().is_none() {
            return Some(KeyStatsPauseReason::NoKeyboard);
        }
        if self.firmware != FirmwareProtocol::Vial {
            return Some(KeyStatsPauseReason::Unsupported);
        }
        if self.is_vial_locked() {
            return Some(KeyStatsPauseReason::Locked);
        }
        let main_window_shows = |main_tab: MainMenuTab, settings_tab: SettingsTab| {
            !self.main_window_hidden_to_tray()
                && self.main_menu_tab == main_tab
                && self.settings_tab == settings_tab
        };
        if main_window_shows(MainMenuTab::Settings, SettingsTab::MatrixTester) {
            return Some(KeyStatsPauseReason::MatrixTester);
        }
        // The whole trainer page counts, including the warm-up keystroke that
        // starts a run: drills are not representative of everyday typing.
        if main_window_shows(MainMenuTab::Advanced, SettingsTab::TypingTrainer) {
            return Some(KeyStatsPauseReason::TypingTrainer);
        }
        None
    }

    /// Loads the statistics of the connected keyboard, saving the previous
    /// keyboard's first when the device changed.
    pub(crate) fn ensure_key_stats_loaded(&mut self) {
        let keyboard_key = self.key_stats_keyboard_key();
        if keyboard_key.is_none() || keyboard_key == self.key_stats.keyboard_key {
            return;
        }
        self.flush_key_stats();
        let store = keyboard_key
            .as_deref()
            .map(|key| load_key_stats(&key_stats_root_dir().join(key)))
            .unwrap_or_default();
        self.key_stats = KeyStatsRuntime {
            store,
            keyboard_key,
            ..KeyStatsRuntime::default()
        };
    }

    pub(crate) fn flush_key_stats(&mut self) {
        let Some(key) = self.key_stats.keyboard_key.clone() else {
            return;
        };
        if self.key_stats.store.has_unsaved_changes() {
            save_key_stats(&key_stats_root_dir().join(key), &mut self.key_stats.store);
        }
        self.key_stats.last_save = Some(std::time::Instant::now());
    }

    fn maybe_autosave_key_stats(&mut self, now: std::time::Instant) {
        let due = self
            .key_stats
            .last_save
            .is_none_or(|last| now.duration_since(last) >= KEY_STATS_AUTOSAVE_INTERVAL);
        if due && self.key_stats.store.has_unsaved_changes() {
            self.flush_key_stats();
        } else if self.key_stats.last_save.is_none() {
            self.key_stats.last_save = Some(now);
        }
    }

    /// Counts the presses of one matrix poll.
    fn log_key_stats_poll_rate(&mut self, presses: usize, now: std::time::Instant) {
        if !self.app_settings.key_heatmap.enabled {
            self.key_stats.rate_window_start = None;
            return;
        }
        let hidden = self.main_window_hidden_to_tray();
        let stats = &mut self.key_stats;
        let start = *stats.rate_window_start.get_or_insert(now);
        stats.rate_polls += 1;
        stats.rate_presses += presses as u32;
        stats.rate_hidden_polls += u32::from(hidden);
        let elapsed = now.duration_since(start);
        if elapsed < KEY_STATS_RATE_LOG_INTERVAL {
            return;
        }
        log::debug!(
            "key heatmap: {:.1} matrix polls/s, {} presses in {:.0}s, {}% of polls hidden to tray",
            stats.rate_polls as f64 / elapsed.as_secs_f64(),
            stats.rate_presses,
            elapsed.as_secs_f64(),
            stats.rate_hidden_polls * 100 / stats.rate_polls.max(1),
        );
        stats.rate_window_start = Some(now);
        stats.rate_polls = 0;
        stats.rate_presses = 0;
        stats.rate_hidden_polls = 0;
    }

    pub(super) fn record_key_stats(
        &mut self,
        presses: &[LayerTrackerPress],
        now: std::time::Instant,
    ) {
        self.log_key_stats_poll_rate(presses.len(), now);
        let collecting = self.key_stats_pause_reason().is_none();
        if !collecting {
            // No route may span a pause: its presses were never seen.
            if self.key_stats.was_collecting {
                self.key_stats.recorder.reset();
                self.key_stats.was_collecting = false;
            }
            return;
        }
        self.ensure_key_stats_loaded();
        self.key_stats.was_collecting = true;

        // Application layouts only swap keymaps, so the physical halves come
        // from the base layout.
        if let Some(layout) = self.layout.as_ref() {
            let halves_source = (layout.name.clone(), layout.keys.len(), layout.cols);
            if self.key_stats.matrix_halves_source.as_ref() != Some(&halves_source) {
                self.key_stats.matrix_halves = matrix_halves(layout);
                self.key_stats.matrix_halves_source = Some(halves_source);
            }
        }
        let settings = self.app_settings.key_heatmap;
        self.key_stats.recorder.max_gap = settings.transition_max_gap();
        self.key_stats.recorder.held_keys_transparent = !settings.show_held_keys_in_routes;

        let date = chrono::Local::now().date_naive();
        for press in presses {
            let (Ok(layer), Ok(matrix_idx)) =
                (u8::try_from(press.layer), u16::try_from(press.matrix_idx))
            else {
                continue;
            };
            let pos = KeyPos::new(layer, matrix_idx);
            let half = self
                .key_stats
                .matrix_halves
                .get(press.matrix_idx)
                .copied()
                .unwrap_or(0);
            self.key_stats.store.record_press(date, pos);
            if let Some((from, to)) =
                self.key_stats
                    .recorder
                    .on_press(pos, half, is_held_keycode(press.keycode), now)
            {
                self.key_stats.store.record_transition(date, from, to);
            }
        }
        self.maybe_autosave_key_stats(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_months_load_back_and_emptied_months_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let october = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let november = chrono::NaiveDate::from_ymd_opt(2026, 11, 3).unwrap();
        let mut store = KeyStatsStore::default();
        store.record_press(october, KeyPos::new(0, 1));
        store.record_press(november, KeyPos::new(1, 2));
        save_key_stats(dir.path(), &mut store);
        assert!(!store.has_unsaved_changes());
        assert!(dir.path().join("2026-10.json").exists());
        assert!(dir.path().join("2026-11.json").exists());

        let mut loaded = load_key_stats(dir.path());
        assert_eq!(loaded, store);

        loaded.clear(Some(crate::key_stats::DateRange::new(november, november)));
        save_key_stats(dir.path(), &mut loaded);
        assert!(dir.path().join("2026-10.json").exists());
        assert!(!dir.path().join("2026-11.json").exists());
    }

    #[test]
    fn unreadable_month_is_set_aside_instead_of_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("2026-10.json"), "{ not json").unwrap();
        std::fs::write(dir.path().join("notes.json"), "{}").unwrap();

        assert!(load_key_stats(dir.path()).is_empty());
        assert!(dir.path().join("2026-10.json.bad").exists());
        assert!(!dir.path().join("2026-10.json").exists());
        assert!(dir.path().join("notes.json").exists());
    }

    #[test]
    fn matrix_halves_follow_matrix_positions() {
        let keys = crate::layouts::parse_embedded_json(crate::layouts::K03_JSON).unwrap();
        let layout = KeyboardLayout {
            name: "K:03".to_string(),
            rows: 10,
            cols: 6,
            keys,
            encoders: vec![],
            layers: vec![],
            encoder_layers: vec![],
            layer_names: vec![],
            custom_keycodes: vec![],
            layout_options: vec![],
            live_features: Default::default(),
            supports_rgb: false,
            lighting_mode: None,
            firmware: FirmwareProtocol::Vial,
        };
        let halves = matrix_halves(&layout);
        assert_eq!(halves.len(), 60);
        assert!(halves[..30].iter().all(|half| *half == 0));
        assert!(halves[30..].iter().all(|half| *half == 1));
    }
}
