//! Keypress heatmap statistics: per-day press counters and same-half
//! transition counters keyed by (layer, physical matrix position).
//!
//! Only aggregates are kept — no event log and no typed text — so the stored
//! data cannot be replayed into what was typed.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use chrono::NaiveDate;

use crate::keyboard::PhysicalKey;

pub(crate) const KEY_STATS_VERSION: u8 = 1;
pub(crate) const DEFAULT_TRANSITION_MAX_GAP: Duration = Duration::from_millis(1000);
/// Smallest horizontal gap between key centers, in KLE units, that separates
/// the halves of a split keyboard.
const SPLIT_GAP_MIN_UNITS: f32 = 1.5;
const DATE_FORMAT: &str = "%Y-%m-%d";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct KeyPos {
    pub(crate) layer: u8,
    /// `row * cols + col`, the same index the switch matrix poll reports.
    pub(crate) matrix_idx: u16,
}

impl KeyPos {
    pub(crate) fn new(layer: u8, matrix_idx: u16) -> Self {
        Self { layer, matrix_idx }
    }
}

/// Inclusive date range; `None` in queries means all time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DateRange {
    pub(crate) start: NaiveDate,
    pub(crate) end: NaiveDate,
}

impl DateRange {
    pub(crate) fn new(a: NaiveDate, b: NaiveDate) -> Self {
        Self {
            start: a.min(b),
            end: a.max(b),
        }
    }

    pub(crate) fn contains(&self, date: NaiveDate) -> bool {
        self.start <= date && date <= self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransitionDirection {
    /// Where the finger goes after the selected key.
    Outgoing,
    /// Where the finger comes from before the selected key.
    Incoming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransitionCount {
    /// The other end of the transition (target when outgoing, source when incoming).
    pub(crate) other: KeyPos,
    pub(crate) count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "DayStatsFile", into = "DayStatsFile")]
pub(crate) struct DayStats {
    presses: HashMap<KeyPos, u32>,
    transitions: HashMap<(KeyPos, KeyPos), u32>,
}

/// JSON object keys must be strings, so counters are stored as compact rows:
/// presses `[layer, matrix_idx, count]`, transitions
/// `[from_layer, from_idx, to_layer, to_idx, count]`.
#[derive(serde::Serialize, serde::Deserialize)]
struct DayStatsFile {
    #[serde(default)]
    presses: Vec<(u8, u16, u32)>,
    #[serde(default)]
    transitions: Vec<(u8, u16, u8, u16, u32)>,
}

impl From<DayStatsFile> for DayStats {
    fn from(file: DayStatsFile) -> Self {
        let mut stats = DayStats::default();
        for (layer, idx, count) in file.presses {
            *stats.presses.entry(KeyPos::new(layer, idx)).or_default() += count;
        }
        for (from_layer, from_idx, to_layer, to_idx, count) in file.transitions {
            *stats
                .transitions
                .entry((
                    KeyPos::new(from_layer, from_idx),
                    KeyPos::new(to_layer, to_idx),
                ))
                .or_default() += count;
        }
        stats
    }
}

impl From<DayStats> for DayStatsFile {
    fn from(stats: DayStats) -> Self {
        let mut presses: Vec<_> = stats
            .presses
            .into_iter()
            .map(|(pos, count)| (pos.layer, pos.matrix_idx, count))
            .collect();
        presses.sort_unstable();
        let mut transitions: Vec<_> = stats
            .transitions
            .into_iter()
            .map(|((from, to), count)| {
                (from.layer, from.matrix_idx, to.layer, to.matrix_idx, count)
            })
            .collect();
        transitions.sort_unstable();
        Self {
            presses,
            transitions,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KeyStatsFile {
    version: u8,
    days: BTreeMap<String, DayStats>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyStatsStore {
    days: BTreeMap<NaiveDate, DayStats>,
    /// Set by every mutation; cleared by the caller after a successful save.
    pub(crate) dirty: bool,
}

fn layer_matches(layer_filter: Option<u8>, layer: u8) -> bool {
    layer_filter.is_none_or(|filter| filter == layer)
}

impl KeyStatsStore {
    pub(crate) fn from_json(data: &str) -> Result<Self, String> {
        let file: KeyStatsFile = serde_json::from_str(data).map_err(|error| error.to_string())?;
        if file.version != KEY_STATS_VERSION {
            return Err(format!("unsupported key stats version: {}", file.version));
        }
        let mut days = BTreeMap::new();
        for (date, stats) in file.days {
            let date = NaiveDate::parse_from_str(&date, DATE_FORMAT)
                .map_err(|error| format!("invalid key stats date {date:?}: {error}"))?;
            days.insert(date, stats);
        }
        Ok(Self { days, dirty: false })
    }

    pub(crate) fn to_json(&self) -> Result<String, String> {
        let file = KeyStatsFile {
            version: KEY_STATS_VERSION,
            days: self
                .days
                .iter()
                .map(|(date, stats)| (date.format(DATE_FORMAT).to_string(), stats.clone()))
                .collect(),
        };
        serde_json::to_string(&file).map_err(|error| error.to_string())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.days.is_empty()
    }

    pub(crate) fn record_press(&mut self, date: NaiveDate, pos: KeyPos) {
        *self
            .days
            .entry(date)
            .or_default()
            .presses
            .entry(pos)
            .or_default() += 1;
        self.dirty = true;
    }

    pub(crate) fn record_transition(&mut self, date: NaiveDate, from: KeyPos, to: KeyPos) {
        *self
            .days
            .entry(date)
            .or_default()
            .transitions
            .entry((from, to))
            .or_default() += 1;
        self.dirty = true;
    }

    fn days_in(&self, range: Option<DateRange>) -> impl Iterator<Item = &DayStats> {
        self.days
            .iter()
            .filter(move |(date, _)| range.is_none_or(|range| range.contains(**date)))
            .map(|(_, stats)| stats)
    }

    /// Press counts per matrix position, summed over the matching layers.
    pub(crate) fn presses(
        &self,
        range: Option<DateRange>,
        layer_filter: Option<u8>,
    ) -> HashMap<u16, u32> {
        let mut totals = HashMap::new();
        for day in self.days_in(range) {
            for (pos, count) in &day.presses {
                if layer_matches(layer_filter, pos.layer) {
                    *totals.entry(pos.matrix_idx).or_default() += count;
                }
            }
        }
        totals
    }

    /// Transitions touching the selected matrix position, most frequent first.
    /// The layer filter applies to the selected key's end of the transition.
    pub(crate) fn transitions(
        &self,
        range: Option<DateRange>,
        layer_filter: Option<u8>,
        selected_matrix_idx: u16,
        direction: TransitionDirection,
    ) -> Vec<TransitionCount> {
        let mut totals: HashMap<KeyPos, u32> = HashMap::new();
        for day in self.days_in(range) {
            for ((from, to), count) in &day.transitions {
                let (selected, other) = match direction {
                    TransitionDirection::Outgoing => (from, to),
                    TransitionDirection::Incoming => (to, from),
                };
                if selected.matrix_idx == selected_matrix_idx
                    && layer_matches(layer_filter, selected.layer)
                {
                    *totals.entry(*other).or_default() += count;
                }
            }
        }
        let mut counts: Vec<_> = totals
            .into_iter()
            .map(|(other, count)| TransitionCount { other, count })
            .collect();
        counts.sort_unstable_by(|a, b| b.count.cmp(&a.count).then(a.other.cmp(&b.other)));
        counts
    }

    /// Total presses per day on the matching layers, for the activity calendar.
    pub(crate) fn daily_totals(&self, layer_filter: Option<u8>) -> BTreeMap<NaiveDate, u32> {
        self.days
            .iter()
            .map(|(date, day)| {
                let total = day
                    .presses
                    .iter()
                    .filter(|(pos, _)| layer_matches(layer_filter, pos.layer))
                    .map(|(_, count)| *count)
                    .sum();
                (*date, total)
            })
            .filter(|(_, total)| *total > 0)
            .collect()
    }

    /// Removes the days in `range`, or everything when `range` is `None`.
    pub(crate) fn clear(&mut self, range: Option<DateRange>) {
        let before = self.days.len();
        match range {
            Some(range) => self.days.retain(|date, _| !range.contains(*date)),
            None => self.days.clear(),
        }
        if self.days.len() != before {
            self.dirty = true;
        }
    }
}

/// Turns a stream of presses into same-half transitions.
///
/// Each half remembers its own last press, so `L1 → R1 → L2` yields `L1 → L2`:
/// the next movement of the left hand, regardless of what the right hand did
/// in between.
#[derive(Clone, Debug)]
pub(crate) struct TransitionRecorder {
    last_press: Vec<Option<(KeyPos, Instant)>>,
    pub(crate) max_gap: Duration,
    /// When set, held keys (layer switches, modifiers) neither start nor
    /// break a route: `a → MO(1) → (` yields `a → (`.
    pub(crate) held_keys_transparent: bool,
}

impl Default for TransitionRecorder {
    fn default() -> Self {
        Self {
            last_press: Vec::new(),
            max_gap: DEFAULT_TRANSITION_MAX_GAP,
            held_keys_transparent: true,
        }
    }
}

impl TransitionRecorder {
    /// Records a press on `half` and returns the transition it completes, if any.
    pub(crate) fn on_press(
        &mut self,
        pos: KeyPos,
        half: u8,
        is_held_key: bool,
        now: Instant,
    ) -> Option<(KeyPos, KeyPos)> {
        if is_held_key && self.held_keys_transparent {
            return None;
        }
        let half = half as usize;
        if self.last_press.len() <= half {
            self.last_press.resize(half + 1, None);
        }
        let previous = self.last_press[half].replace((pos, now));
        previous
            .filter(|(_, at)| now.saturating_duration_since(*at) <= self.max_gap)
            .map(|(from, _)| (from, pos))
    }

    /// Forgets the last presses, e.g. after a pause in collection, so no
    /// transition spans the gap.
    pub(crate) fn reset(&mut self) {
        self.last_press.clear();
    }
}

/// Keys that are held while other keys are pressed rather than typed:
/// modifiers, momentary layer switches and one-shot keys. Dual-role keys
/// (mod-tap, layer-tap) are typed when tapped, so they are not included.
pub(crate) fn is_held_keycode(keycode: u16) -> bool {
    matches!(
        keycode,
        0x00E0..=0x00E7 // KC_LCTL..KC_RGUI
            | 0x5000..=0x51FF // LM(layer, mods)
            | 0x5220..=0x523F // MO(layer)
            | 0x5280..=0x529F // OSL(layer)
            | 0x52A0..=0x52BF // OSM(mods)
            | 0x52C0..=0x52DF // TT(layer)
    )
}

fn key_center(key: &PhysicalKey) -> (f32, f32) {
    let x = key.x + key.w * 0.5;
    let y = key.y + key.h * 0.5;
    if key.rotation == 0.0 {
        return (x, y);
    }
    let angle = key.rotation.to_radians();
    let dx = x - key.rotation_x;
    let dy = y - key.rotation_y;
    (
        key.rotation_x + dx * angle.cos() - dy * angle.sin(),
        key.rotation_y + dx * angle.sin() + dy * angle.cos(),
    )
}

/// Assigns every key to a keyboard half (0 = left, 1 = right).
///
/// Split boards are recognised by the widest horizontal gap between key
/// centers; a board without such a gap is a single half.
pub(crate) fn split_halves(keys: &[PhysicalKey]) -> Vec<u8> {
    let centers: Vec<f32> = keys.iter().map(|key| key_center(key).0).collect();
    let mut sorted = centers.clone();
    sorted.sort_by(f32::total_cmp);
    let split_x = sorted
        .windows(2)
        .map(|pair| (pair[1] - pair[0], (pair[0] + pair[1]) * 0.5))
        .filter(|(gap, _)| *gap >= SPLIT_GAP_MIN_UNITS)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, mid)| mid);
    match split_x {
        Some(split_x) => centers.iter().map(|x| u8::from(*x > split_x)).collect(),
        None => vec![0; keys.len()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    fn key(x: f32, y: f32) -> PhysicalKey {
        PhysicalKey {
            x,
            y,
            w: 1.0,
            h: 1.0,
            row: 0,
            col: 0,
            label: String::new(),
            rotation: 0.0,
            rotation_x: 0.0,
            rotation_y: 0.0,
            layout_condition: None,
        }
    }

    const L1: KeyPos = KeyPos {
        layer: 0,
        matrix_idx: 1,
    };
    const L2: KeyPos = KeyPos {
        layer: 0,
        matrix_idx: 2,
    };
    const R1: KeyPos = KeyPos {
        layer: 0,
        matrix_idx: 31,
    };

    #[test]
    fn transitions_follow_each_half_independently() {
        let mut recorder = TransitionRecorder::default();
        let t0 = Instant::now();
        assert_eq!(recorder.on_press(L1, 0, false, t0), None);
        assert_eq!(
            recorder.on_press(R1, 1, false, t0 + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            recorder.on_press(L2, 0, false, t0 + Duration::from_millis(200)),
            Some((L1, L2))
        );
    }

    #[test]
    fn pause_longer_than_max_gap_breaks_the_route() {
        let mut recorder = TransitionRecorder::default();
        let t0 = Instant::now();
        recorder.on_press(L1, 0, false, t0);
        assert_eq!(
            recorder.on_press(L2, 0, false, t0 + Duration::from_millis(1001)),
            None
        );
        assert_eq!(
            recorder.on_press(L1, 0, false, t0 + Duration::from_millis(2000)),
            Some((L2, L1))
        );
    }

    #[test]
    fn repeated_key_is_a_transition_to_itself() {
        let mut recorder = TransitionRecorder::default();
        let t0 = Instant::now();
        recorder.on_press(L1, 0, false, t0);
        assert_eq!(
            recorder.on_press(L1, 0, false, t0 + Duration::from_millis(80)),
            Some((L1, L1))
        );
    }

    #[test]
    fn held_keys_are_transparent_only_when_enabled() {
        let held = KeyPos::new(0, 4);
        let symbol = KeyPos::new(1, 2);
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);

        let mut recorder = TransitionRecorder::default();
        recorder.on_press(L1, 0, false, ms(0));
        assert_eq!(recorder.on_press(held, 0, true, ms(50)), None);
        assert_eq!(
            recorder.on_press(symbol, 0, false, ms(100)),
            Some((L1, symbol))
        );

        let mut recorder = TransitionRecorder {
            held_keys_transparent: false,
            ..TransitionRecorder::default()
        };
        recorder.on_press(L1, 0, false, ms(0));
        assert_eq!(recorder.on_press(held, 0, true, ms(50)), Some((L1, held)));
        assert_eq!(
            recorder.on_press(symbol, 0, false, ms(100)),
            Some((held, symbol))
        );
    }

    #[test]
    fn reset_drops_pending_routes() {
        let mut recorder = TransitionRecorder::default();
        let t0 = Instant::now();
        recorder.on_press(L1, 0, false, t0);
        recorder.reset();
        assert_eq!(recorder.on_press(L2, 0, false, t0), None);
    }

    #[test]
    fn held_keycodes_cover_modifiers_and_momentary_layers_only() {
        for held in [
            0x00E0, 0x00E7, 0x5221, 0x5224, 0x5281, 0x52A1, 0x52C1, 0x5012,
        ] {
            assert!(is_held_keycode(held), "{held:#06x} should be held");
        }
        // KC_A, LT(1, KC_SPC), LCTL_T(KC_A), TG(1), TO(1), LSFT(KC_9)
        for typed in [0x0004, 0x412C, 0x2104, 0x5261, 0x5201, 0x0226] {
            assert!(!is_held_keycode(typed), "{typed:#06x} should not be held");
        }
    }

    #[test]
    fn k03_splits_into_left_and_right_rows() {
        let keys = crate::layouts::parse_embedded_json(crate::layouts::K03_JSON)
            .expect("K:03 layout should parse");
        let halves = split_halves(&keys);
        for (key, half) in keys.iter().zip(&halves) {
            let expected = u8::from(key.row >= 5);
            assert_eq!(*half, expected, "K:03 key r{}c{}", key.row, key.col);
        }
    }

    #[test]
    fn board_without_gap_is_a_single_half() {
        let keys: Vec<_> = (0..12).map(|i| key(i as f32 * 1.25, 0.0)).collect();
        assert_eq!(split_halves(&keys), vec![0; 12]);
    }

    #[test]
    fn queries_filter_by_range_and_layer() {
        let mut store = KeyStatsStore::default();
        store.record_press(date(1), KeyPos::new(0, 1));
        store.record_press(date(1), KeyPos::new(1, 1));
        store.record_press(date(2), KeyPos::new(0, 1));
        store.record_press(date(3), KeyPos::new(0, 2));

        let all = store.presses(None, None);
        assert_eq!(all.get(&1), Some(&3));
        assert_eq!(all.get(&2), Some(&1));

        let first_two_days = Some(DateRange::new(date(2), date(1)));
        assert_eq!(store.presses(first_two_days, Some(0)).get(&1), Some(&2));
        assert_eq!(store.presses(first_two_days, Some(1)).get(&1), Some(&1));
        assert_eq!(store.presses(first_two_days, None).get(&2), None);

        assert_eq!(store.daily_totals(Some(1)), BTreeMap::from([(date(1), 1)]));
        assert_eq!(
            store.daily_totals(None),
            BTreeMap::from([(date(1), 2), (date(2), 1), (date(3), 1)])
        );
    }

    #[test]
    fn transitions_are_ranked_and_filtered_by_the_selected_key_layer() {
        let mut store = KeyStatsStore::default();
        let a0 = KeyPos::new(0, 1);
        let a1 = KeyPos::new(1, 1);
        let b0 = KeyPos::new(0, 2);
        let c0 = KeyPos::new(0, 3);
        store.record_transition(date(1), a0, b0);
        store.record_transition(date(1), a0, c0);
        store.record_transition(date(2), a0, c0);
        store.record_transition(date(2), a1, b0);

        let outgoing = store.transitions(None, Some(0), 1, TransitionDirection::Outgoing);
        assert_eq!(
            outgoing,
            vec![
                TransitionCount {
                    other: c0,
                    count: 2
                },
                TransitionCount {
                    other: b0,
                    count: 1
                },
            ]
        );

        // a0 and a1 are the same physical key, so both feed the all-layers view;
        // equal counts fall back to position order.
        let all_layers = store.transitions(None, None, 1, TransitionDirection::Outgoing);
        assert_eq!(
            all_layers,
            vec![
                TransitionCount {
                    other: b0,
                    count: 2
                },
                TransitionCount {
                    other: c0,
                    count: 2
                },
            ]
        );

        let incoming = store.transitions(None, None, 2, TransitionDirection::Incoming);
        assert_eq!(
            incoming,
            vec![
                TransitionCount {
                    other: a0,
                    count: 1
                },
                TransitionCount {
                    other: a1,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn json_round_trip_preserves_counters() {
        let mut store = KeyStatsStore::default();
        store.record_press(date(1), KeyPos::new(2, 17));
        store.record_press(date(1), KeyPos::new(2, 17));
        store.record_transition(date(5), KeyPos::new(0, 1), KeyPos::new(1, 2));

        let json = store.to_json().unwrap();
        let mut loaded = KeyStatsStore::from_json(&json).unwrap();
        assert!(!loaded.dirty);
        loaded.dirty = true;
        assert_eq!(loaded, store);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        assert!(KeyStatsStore::from_json(r#"{"version":99,"days":{}}"#).is_err());
    }

    #[test]
    fn clear_removes_only_the_selected_days() {
        let mut store = KeyStatsStore::default();
        for day in 1..=5 {
            store.record_press(date(day), KeyPos::new(0, 1));
        }
        store.dirty = false;
        store.clear(Some(DateRange::new(date(2), date(4))));
        assert!(store.dirty);
        assert_eq!(
            store.daily_totals(None).keys().copied().collect::<Vec<_>>(),
            vec![date(1), date(5)]
        );
        store.clear(None);
        assert!(store.is_empty());
    }
}
