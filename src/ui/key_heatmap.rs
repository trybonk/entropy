use super::*;
use crate::key_stats::{split_halves, DateRange, KeyPos, TransitionCount, TransitionDirection};
use chrono::Datelike;
use key_heatmap_runtime::KeyStatsPauseReason;

/// Spread of one key's heat, in key units.
const HEAT_SIGMA_UNITS: f32 = 0.38;
/// Heat stays fully visible this far from the nearest key center...
const HEAT_SOLID_UNITS: f32 = 0.62;
/// ...and fades out completely here, about 0.7u beyond the cap edge.
const HEAT_FADE_UNITS: f32 = 1.05;
/// Screen pixels per heat texture pixel; the texture is smoothed when drawn.
const HEAT_PIXEL_SIZE: f32 = 2.0;
/// Rebuilding the heat texture costs a few milliseconds, so live typing
/// refreshes it at most this often.
const HEAT_REBUILD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);
/// Heat is dimmed to this gray tint while routes are drawn on top of it.
const HEAT_DIM_TINT: u8 = 105;
const ROUTE_COLOR: Color32 = Color32::from_rgb(120, 222, 255);
const ROUTE_MIN_WIDTH: f32 = 1.5;
const ROUTE_MAX_WIDTH: f32 = 8.0;
/// Route limits offered next to the table; 0 shows every route.
const TOP_TRANSITION_CHOICES: [u8; 4] = [3, 5, 10, 0];

/// "Ironbow" thermal camera palette, cold to hot.
const IRONBOW_STOPS: [(f32, [u8; 3]); 7] = [
    (0.00, [8, 6, 30]),
    (0.15, [40, 10, 120]),
    (0.35, [140, 20, 150]),
    (0.55, [222, 60, 45]),
    (0.72, [247, 132, 27]),
    (0.88, [255, 212, 59]),
    (1.00, [255, 252, 235]),
];

pub(crate) fn ironbow(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let upper = IRONBOW_STOPS
        .iter()
        .position(|(stop, _)| *stop >= t)
        .unwrap_or(IRONBOW_STOPS.len() - 1)
        .max(1);
    let (t0, c0) = IRONBOW_STOPS[upper - 1];
    let (t1, c1) = IRONBOW_STOPS[upper];
    let k = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * k).round() as u8;
    Color32::from_rgb(lerp(c0[0], c1[0]), lerp(c0[1], c1[1]), lerp(c0[2], c1[2]))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyHeatmapPeriod {
    ThisWeek,
    LastWeek,
    AllTime,
    /// Days picked in the activity calendar.
    Custom(DateRange),
}

impl KeyHeatmapPeriod {
    const PRESETS: [Self; 3] = [Self::ThisWeek, Self::LastWeek, Self::AllTime];

    fn preset_label_key(self) -> &'static str {
        match self {
            Self::ThisWeek => "key_heatmap.this_week",
            Self::LastWeek => "key_heatmap.last_week",
            Self::AllTime | Self::Custom(_) => "key_heatmap.all_time",
        }
    }

    /// Weeks start on Monday, in local time.
    pub(crate) fn range(self, today: chrono::NaiveDate) -> Option<DateRange> {
        let monday = today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
        match self {
            Self::ThisWeek => Some(DateRange::new(monday, today)),
            Self::LastWeek => Some(DateRange::new(
                monday - chrono::Days::new(7),
                monday - chrono::Days::new(1),
            )),
            Self::AllTime => None,
            Self::Custom(range) => Some(range),
        }
    }
}

fn short_date(date: chrono::NaiveDate) -> String {
    date.format("%d.%m").to_string()
}

fn custom_period_label(range: DateRange) -> String {
    if range.start == range.end {
        short_date(range.start)
    } else {
        format!("{}–{}", short_date(range.start), short_date(range.end))
    }
}

/// Days in the calendar: whole weeks (Monday first), the last one holding today.
const CALENDAR_WEEKS: u64 = 53;

fn calendar_first_day(today: chrono::NaiveDate) -> chrono::NaiveDate {
    let monday = today - chrono::Days::new(today.weekday().num_days_from_monday() as u64);
    monday - chrono::Days::new((CALENDAR_WEEKS - 1) * 7)
}

/// The range a calendar click selects: a single day, or with Shift the span
/// from the previously clicked day.
fn calendar_click_range(
    anchor: Option<chrono::NaiveDate>,
    clicked: chrono::NaiveDate,
    extend: bool,
) -> DateRange {
    match anchor.filter(|_| extend) {
        Some(anchor) => DateRange::new(anchor, clicked),
        None => DateRange::new(clicked, clicked),
    }
}

pub(crate) struct KeyHeatmapPageState {
    /// `None` shows all layers summed by physical key.
    pub(crate) layer: Option<usize>,
    pub(crate) period: KeyHeatmapPeriod,
    /// Matrix position whose routes are drawn.
    pub(crate) selected_key: Option<u16>,
    pub(crate) direction: TransitionDirection,
    /// Day the last plain calendar click picked; Shift+click extends from it.
    calendar_anchor: Option<chrono::NaiveDate>,
    clear_dialog_open: bool,
    heat_texture: Option<(u64, egui::TextureHandle)>,
    heat_built_at: Option<std::time::Instant>,
}

impl Default for KeyHeatmapPageState {
    fn default() -> Self {
        Self {
            layer: None,
            period: KeyHeatmapPeriod::ThisWeek,
            selected_key: None,
            direction: TransitionDirection::Outgoing,
            calendar_anchor: None,
            clear_dialog_open: false,
            heat_texture: None,
            heat_built_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct HeatSource {
    center: egui::Pos2,
    half: u8,
    /// Normalized intensity in `0.0..=1.0`.
    weight: f32,
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Renders the heat field over `area`. Each pixel takes its keyboard half
/// from the nearest key, so heat never bleeds across the gap of a split
/// keyboard.
fn render_heat_image(area: egui::Rect, unit: f32, sources: &[HeatSource]) -> egui::ColorImage {
    let width = (area.width() / HEAT_PIXEL_SIZE).ceil().max(1.0) as usize;
    let height = (area.height() / HEAT_PIXEL_SIZE).ceil().max(1.0) as usize;
    let mut image = egui::ColorImage::filled([width, height], Color32::TRANSPARENT);
    if sources.is_empty() || unit <= 0.0 {
        return image;
    }
    let sigma = HEAT_SIGMA_UNITS * unit;
    let reach_sq = (3.0 * sigma).powi(2);
    let inv_two_sigma_sq = 1.0 / (2.0 * sigma * sigma);
    let solid = HEAT_SOLID_UNITS * unit;
    let fade = HEAT_FADE_UNITS * unit;

    for y in 0..height {
        for x in 0..width {
            let pos = area.min
                + egui::vec2(
                    (x as f32 + 0.5) * HEAT_PIXEL_SIZE,
                    (y as f32 + 0.5) * HEAT_PIXEL_SIZE,
                );
            let Some((nearest, nearest_sq)) = sources
                .iter()
                .map(|source| (source, (source.center - pos).length_sq()))
                .min_by(|a, b| a.1.total_cmp(&b.1))
            else {
                continue;
            };
            let alpha = 1.0 - smoothstep(solid, fade, nearest_sq.sqrt());
            if alpha <= 0.0 {
                continue;
            }
            let heat: f32 = sources
                .iter()
                .filter(|source| source.half == nearest.half && source.weight > 0.0)
                .map(|source| {
                    let distance_sq = (source.center - pos).length_sq();
                    if distance_sq > reach_sq {
                        0.0
                    } else {
                        source.weight * (-distance_sq * inv_two_sigma_sq).exp()
                    }
                })
                .sum();
            let color = ironbow(heat.min(1.0));
            image.pixels[y * width + x] = Color32::from_rgba_unmultiplied(
                color.r(),
                color.g(),
                color.b(),
                (alpha * 255.0).round() as u8,
            );
        }
    }
    image
}

/// Normalized key intensity: logarithmic by default so a few very busy keys
/// (space, E) do not flatten everything else into the coldest color.
fn heat_weight(count: u32, max_count: u32, log_scale: bool) -> f32 {
    if count == 0 || max_count == 0 {
        return 0.0;
    }
    if log_scale {
        (count as f32).ln_1p() / (max_count as f32).ln_1p()
    } else {
        count as f32 / max_count as f32
    }
}

/// Route totals per physical key, most frequent first, limited to `top`
/// entries (0 keeps all). Layers of the other key are summed.
fn physical_routes(routes: &[TransitionCount], top: u8) -> Vec<(u16, u32)> {
    let mut totals: Vec<(u16, u32)> = Vec::new();
    for route in routes {
        match totals
            .iter_mut()
            .find(|(idx, _)| *idx == route.other.matrix_idx)
        {
            Some((_, count)) => *count += route.count,
            None => totals.push((route.other.matrix_idx, route.count)),
        }
    }
    totals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if top > 0 {
        totals.truncate(top as usize);
    }
    totals
}

fn route_width(count: u32, max_count: u32) -> f32 {
    if max_count == 0 {
        return ROUTE_MIN_WIDTH;
    }
    ROUTE_MIN_WIDTH + (ROUTE_MAX_WIDTH - ROUTE_MIN_WIDTH) * count as f32 / max_count as f32
}

/// Curved arrow between two key centers, trimmed to the cap edges. The bend
/// keeps `a → b` and `b → a` apart when both are shown.
fn paint_route_arrow(
    painter: &egui::Painter,
    from: egui::Pos2,
    to: egui::Pos2,
    width: f32,
    key_radius: f32,
) {
    let delta = to - from;
    let length = delta.length();
    if length <= key_radius * 2.0 {
        return;
    }
    let dir = delta / length;
    let normal = egui::vec2(-dir.y, dir.x);
    let start = from + dir * key_radius;
    let end = to - dir * key_radius;
    let control = start + (end - start) * 0.5 + normal * (length * 0.18);
    let head_len = (width * 2.6).max(9.0);
    let points: Vec<egui::Pos2> = (0..=24)
        .map(|i| {
            let t = i as f32 / 24.0;
            let u = 1.0 - t;
            (start.to_vec2() * (u * u)
                + control.to_vec2() * (2.0 * u * t)
                + end.to_vec2() * (t * t))
                .to_pos2()
        })
        .collect();
    let tangent = (end - control).normalized();
    let head_base = end - tangent * head_len;
    let head_normal = egui::vec2(-tangent.y, tangent.x) * (head_len * 0.55);
    let shaft: Vec<egui::Pos2> = points
        .into_iter()
        .take_while(|point| (*point - start).length() <= (head_base - start).length())
        .chain(std::iter::once(head_base))
        .collect();
    let head = vec![end, head_base + head_normal, head_base - head_normal];

    let shadow = Color32::from_black_alpha(140);
    painter.add(egui::Shape::line(
        shaft.clone(),
        Stroke::new(width + 2.0, shadow),
    ));
    painter.add(egui::Shape::convex_polygon(
        head.clone(),
        shadow,
        Stroke::new(2.0, shadow),
    ));
    painter.add(egui::Shape::line(shaft, Stroke::new(width, ROUTE_COLOR)));
    painter.add(egui::Shape::convex_polygon(head, ROUTE_COLOR, Stroke::NONE));
}

fn pause_reason_key(reason: Option<KeyStatsPauseReason>) -> &'static str {
    match reason {
        None => "key_heatmap.status_collecting",
        Some(KeyStatsPauseReason::Disabled) => "key_heatmap.status_disabled",
        Some(KeyStatsPauseReason::NoKeyboard) => "key_heatmap.status_no_keyboard",
        Some(KeyStatsPauseReason::Unsupported) => "key_heatmap.status_unsupported",
        Some(KeyStatsPauseReason::Locked) => "key_heatmap.status_locked",
        Some(KeyStatsPauseReason::MatrixTester) => "key_heatmap.status_matrix_tester",
        Some(KeyStatsPauseReason::TypingTrainer) => "key_heatmap.status_typing_trainer",
    }
}

impl EntropyApp {
    pub(super) fn open_key_heatmap_page(&mut self) {
        self.settings_tab = SettingsTab::KeyHeatmap;
        self.main_menu_tab = MainMenuTab::Advanced;
    }

    fn key_heatmap_layer_name(&self, layer: Option<usize>) -> String {
        let lang = self.app_settings.language;
        match layer {
            None => crate::i18n::tr_catalog(lang, "key_heatmap.all_layers").to_string(),
            Some(layer) => match self.layer_names.get(layer).map(|name| name.trim()) {
                Some(name) if !name.is_empty() && name != layer.to_string() => {
                    format!("{layer}. {}", name.chars().take(12).collect::<String>())
                }
                _ => layer.to_string(),
            },
        }
    }

    fn draw_key_heatmap_layer_switcher(&mut self, ui: &mut egui::Ui, center: egui::Pos2) {
        let dark = ui.visuals().dark_mode;
        let layer_count = self
            .layout
            .as_ref()
            .map(|layout| layout.layers.len())
            .unwrap_or(0)
            .max(1);
        // Position 0 is "all layers", then every layer in order.
        let position = self.key_heatmap_page.layer.map_or(0, |layer| layer + 1);
        let positions = layer_count + 1;
        let set_position = |state: &mut KeyHeatmapPageState, position: usize| {
            state.layer = position.checked_sub(1);
        };

        let name = self.key_heatmap_layer_name(self.key_heatmap_page.layer);
        let text_color = if dark {
            Color32::from_gray(245)
        } else {
            Color32::from_gray(60)
        };
        let left_center = center - egui::vec2(120.0, 2.0);
        let right_center = center + egui::vec2(120.0, -2.0);
        let left_hit = egui::Rect::from_center_size(left_center, Vec2::new(28.0, 40.0));
        let right_hit = egui::Rect::from_center_size(right_center, Vec2::new(28.0, 40.0));
        let wheel_hit = egui::Rect::from_min_max(
            egui::pos2(left_hit.left(), center.y - 20.0),
            egui::pos2(right_hit.right(), center.y + 20.0),
        );
        let wheel = ui.interact(
            wheel_hit,
            ui.id().with("key_heatmap_layer_wheel"),
            Sense::hover(),
        );
        if wheel.hovered() {
            let scroll = ui.input(|i| {
                i.raw
                    .events
                    .iter()
                    .find_map(|event| match event {
                        egui::Event::MouseWheel { delta, .. } => Some(delta.y),
                        _ => None,
                    })
                    .unwrap_or(0.0)
            });
            let next = layout_layer_switcher::layer_after_wheel(position, positions, scroll);
            if next != position {
                set_position(&mut self.key_heatmap_page, next);
            }
        }
        let left = ui.interact(
            left_hit,
            ui.id().with("key_heatmap_layer_prev"),
            Sense::click(),
        );
        let right = ui.interact(
            right_hit,
            ui.id().with("key_heatmap_layer_next"),
            Sense::click(),
        );
        if left.clicked() && position > 0 {
            set_position(&mut self.key_heatmap_page, position - 1);
        }
        if right.clicked() && position + 1 < positions {
            set_position(&mut self.key_heatmap_page, position + 1);
        }
        for response in [&left, &right] {
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        let arrow_color = |hovered: bool, enabled: bool| {
            if !enabled {
                if dark {
                    Color32::from_gray(60)
                } else {
                    Color32::from_gray(200)
                }
            } else if hovered {
                app_accent()
            } else if dark {
                Color32::from_gray(140)
            } else {
                Color32::from_gray(120)
            }
        };
        let painter = ui.painter();
        painter.text(
            left_center,
            egui::Align2::CENTER_CENTER,
            "‹",
            FontId::proportional(40.0),
            arrow_color(left.hovered(), position > 0),
        );
        painter.text(
            right_center,
            egui::Align2::CENTER_CENTER,
            "›",
            FontId::proportional(40.0),
            arrow_color(right.hovered(), position + 1 < positions),
        );
        painter.text(
            center,
            egui::Align2::CENTER_CENTER,
            name,
            FontId::proportional(24.0),
            text_color,
        );
    }

    pub(super) fn draw_key_heatmap_page(
        &mut self,
        ui: &mut egui::Ui,
        layout: &KeyboardLayout,
        content_rect: egui::Rect,
        dark: bool,
    ) {
        let lang = self.app_settings.language;
        self.ensure_key_stats_loaded();
        if let Some(layer) = self.key_heatmap_page.layer {
            if layer >= layout.layers.len().max(1) {
                self.key_heatmap_page.layer = None;
            }
        }

        let title_y = content_rect.top() + 30.0;
        let controls_y = title_y + 60.0;
        let layer_y = controls_y + 46.0;
        crate::ui_style::allocate_ui_at_rect(
            ui,
            egui::Rect::from_min_max(
                content_rect.left_top(),
                egui::pos2(content_rect.right(), title_y + 48.0),
            ),
            |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(18.0);
                    ui.label(
                        RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.title"))
                            .size(18.0)
                            .strong(),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.description"))
                            .size(13.0)
                            .color(app_muted_text(dark)),
                    );
                });
            },
        );

        // Controls: collection switch with its status, period and scale.
        let mut period_labels: Vec<String> = KeyHeatmapPeriod::PRESETS
            .iter()
            .map(|period| crate::i18n::tr_catalog(lang, period.preset_label_key()).to_string())
            .collect();
        if let KeyHeatmapPeriod::Custom(range) = self.key_heatmap_page.period {
            period_labels.push(custom_period_label(range));
        }
        let scale_labels = vec![
            crate::i18n::tr_catalog(lang, "key_heatmap.scale_log").to_string(),
            crate::i18n::tr_catalog(lang, "key_heatmap.scale_linear").to_string(),
        ];
        let controls_rect = egui::Rect::from_center_size(
            egui::pos2(content_rect.center().x, controls_y),
            Vec2::new(content_rect.width().min(860.0), 30.0),
        );
        let mut settings_changed = false;
        crate::ui_style::allocate_ui_at_rect(ui, controls_rect, |ui| {
            ui.horizontal_centered(|ui| {
                let mut enabled = self.app_settings.key_heatmap.enabled;
                let switch = crate::ui_style::settings_switch(ui, &mut enabled)
                    .on_hover_text(crate::i18n::tr_catalog(lang, "key_heatmap.collect_tooltip"));
                if switch.changed() || enabled != self.app_settings.key_heatmap.enabled {
                    self.app_settings.key_heatmap.enabled = enabled;
                    settings_changed = true;
                }
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.collect")).size(13.0),
                );
                let reason = self.key_stats_pause_reason();
                let status_color = if reason.is_none() {
                    app_accent()
                } else {
                    app_muted_text(dark)
                };
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, pause_reason_key(reason)))
                        .size(12.0)
                        .color(status_color),
                );
                ui.add_space(16.0);
                // A calendar selection shows up as a fourth, selected segment.
                let selected_period = KeyHeatmapPeriod::PRESETS
                    .iter()
                    .position(|period| *period == self.key_heatmap_page.period)
                    .unwrap_or(KeyHeatmapPeriod::PRESETS.len());
                let period_width = 100.0 * period_labels.len() as f32;
                if let Some(picked) = crate::ui_style::settings_segmented_control(
                    ui,
                    "key_heatmap_period",
                    &period_labels,
                    selected_period,
                    Vec2::new(period_width, 28.0),
                ) {
                    if let Some(preset) = KeyHeatmapPeriod::PRESETS.get(picked) {
                        self.key_heatmap_page.period = *preset;
                    }
                }
                ui.add_space(8.0);
                let selected_scale = usize::from(!self.app_settings.key_heatmap.log_scale);
                if let Some(picked) = crate::ui_style::settings_segmented_control(
                    ui,
                    "key_heatmap_scale",
                    &scale_labels,
                    selected_scale,
                    Vec2::new(140.0, 28.0),
                ) {
                    self.app_settings.key_heatmap.log_scale = picked == 0;
                    settings_changed = true;
                }
            });
        });
        if settings_changed {
            save_app_settings(&self.app_settings);
        }

        self.draw_key_heatmap_layer_switcher(ui, egui::pos2(content_rect.center().x, layer_y));

        // Bottom band: activity calendar on the left, routes table on the right.
        let band_bottom = ui.max_rect().bottom() - 14.0;
        let band_top = band_bottom - 160.0;
        let board_rect = egui::Rect::from_min_max(
            egui::pos2(content_rect.left(), layer_y + 28.0),
            egui::pos2(content_rect.right(), band_top - 8.0),
        );
        let table_rect = egui::Rect::from_min_max(
            egui::pos2(content_rect.right() - 320.0, band_top),
            egui::pos2(content_rect.right() - 16.0, band_bottom),
        );
        let calendar_rect = egui::Rect::from_min_max(
            egui::pos2(content_rect.left() + 16.0, band_top),
            egui::pos2(table_rect.left() - 24.0, band_bottom),
        );
        self.draw_key_heatmap_board(ui, layout, board_rect, table_rect, dark);
        self.draw_key_heatmap_calendar(ui, calendar_rect, dark);
        self.draw_key_heatmap_clear_dialog(ui.ctx(), dark);
    }

    fn draw_key_heatmap_calendar(&mut self, ui: &mut egui::Ui, rect: egui::Rect, dark: bool) {
        let lang = self.app_settings.language;
        let painter = ui.painter().clone();
        let today = chrono::Local::now().date_naive();
        let first_day = calendar_first_day(today);
        let layer_filter = self
            .key_heatmap_page
            .layer
            .and_then(|layer| u8::try_from(layer).ok());
        let totals = self.key_stats.store.daily_totals(layer_filter);
        let max_total = totals.values().copied().max().unwrap_or(0);
        let selected_range = self.key_heatmap_page.period.range(today);

        let label_width = 26.0;
        let header_height = 16.0;
        let footer_height = 30.0;
        let gap = 2.0;
        let cell = ((rect.width() - label_width) / CALENDAR_WEEKS as f32 - gap)
            .min((rect.height() - header_height - footer_height) / 7.0 - gap)
            .clamp(4.0, 14.0);
        let step = cell + gap;
        let grid_origin = egui::pos2(rect.left() + label_width, rect.top() + header_height);

        let months: Vec<&str> = crate::i18n::tr_catalog(lang, "key_heatmap.months")
            .split(',')
            .map(str::trim)
            .collect();
        let weekdays: Vec<&str> = crate::i18n::tr_catalog(lang, "key_heatmap.weekdays")
            .split(',')
            .map(str::trim)
            .collect();
        let muted = app_muted_text(dark);
        for row in [0_usize, 2, 4] {
            if let Some(name) = weekdays.get(row) {
                painter.text(
                    egui::pos2(rect.left(), grid_origin.y + row as f32 * step + cell * 0.5),
                    egui::Align2::LEFT_CENTER,
                    *name,
                    FontId::proportional(9.5),
                    muted,
                );
            }
        }

        let empty_fill = if dark {
            Color32::from_rgb(44, 44, 48)
        } else {
            Color32::from_rgb(232, 232, 236)
        };
        let shift_held = ui.input(|i| i.modifiers.shift);
        let mut clicked_day = None;
        let mut previous_month = None;
        for week in 0..CALENDAR_WEEKS {
            let week_start = first_day + chrono::Days::new(week * 7);
            let x = grid_origin.x + week as f32 * step;
            if previous_month != Some(week_start.month()) {
                previous_month = Some(week_start.month());
                // Skip a label squeezed against the left edge by a partial month.
                if week > 0 || week_start.day() <= 7 {
                    if let Some(name) = months.get(week_start.month0() as usize) {
                        painter.text(
                            egui::pos2(x, rect.top()),
                            egui::Align2::LEFT_TOP,
                            *name,
                            FontId::proportional(9.5),
                            muted,
                        );
                    }
                }
            }
            for weekday in 0..7_u64 {
                let date = week_start + chrono::Days::new(weekday);
                if date > today {
                    break;
                }
                let cell_rect = egui::Rect::from_min_size(
                    egui::pos2(x, grid_origin.y + weekday as f32 * step),
                    Vec2::splat(cell),
                );
                let total = totals.get(&date).copied().unwrap_or(0);
                let fill = if total == 0 {
                    empty_fill
                } else {
                    ironbow(0.18 + 0.82 * heat_weight(total, max_total, true))
                };
                painter.rect_filled(cell_rect, 2.0, fill);
                if selected_range.is_some_and(|range| range.contains(date)) {
                    painter.rect_stroke(
                        cell_rect.expand(0.5),
                        2.0,
                        Stroke::new(1.4_f32, ROUTE_COLOR),
                        egui::StrokeKind::Outside,
                    );
                }
                let response = ui.interact(
                    cell_rect,
                    ui.id().with(("key_heatmap_day", date.num_days_from_ce())),
                    Sense::click(),
                );
                if response.clicked() {
                    clicked_day = Some(date);
                }
                response.on_hover_text(crate::i18n::tr_catalog_format(
                    lang,
                    "key_heatmap.day_tooltip",
                    &[
                        ("date", &date.format("%d.%m.%Y").to_string()),
                        ("count", &total.to_string()),
                    ],
                ));
            }
        }
        if let Some(day) = clicked_day {
            let range =
                calendar_click_range(self.key_heatmap_page.calendar_anchor, day, shift_held);
            if !shift_held {
                self.key_heatmap_page.calendar_anchor = Some(day);
            }
            self.key_heatmap_page.period = KeyHeatmapPeriod::Custom(range);
        }

        let footer = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.bottom() - footer_height + 6.0),
            rect.right_bottom(),
        );
        crate::ui_style::allocate_ui_at_rect(ui, footer, |ui| {
            ui.horizontal(|ui| {
                if crate::ui_style::modern_button(
                    ui,
                    crate::i18n::tr_catalog(lang, "key_heatmap.clear"),
                    Vec2::new(150.0, 24.0),
                    !self.key_stats.store.is_empty(),
                )
                .clicked()
                {
                    self.key_heatmap_page.clear_dialog_open = true;
                }
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.calendar_hint"))
                        .size(11.0)
                        .color(muted),
                );
            });
        });
    }

    fn draw_key_heatmap_clear_dialog(&mut self, ctx: &egui::Context, dark: bool) {
        if !self.key_heatmap_page.clear_dialog_open {
            return;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.key_heatmap_page.clear_dialog_open = false;
            return;
        }
        let lang = self.app_settings.language;
        let screen_rect = ctx.content_rect();
        egui::Area::new("key_heatmap_clear_backdrop".into())
            .order(egui::Order::Foreground)
            .fixed_pos(screen_rect.min)
            .show(ctx, |ui| {
                let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, screen_rect.size());
                ui.interact(
                    rect,
                    egui::Id::new("key_heatmap_clear_backdrop_blocker"),
                    egui::Sense::click_and_drag(),
                );
                ui.painter().rect_filled(
                    rect,
                    0.0,
                    Color32::from_black_alpha(crate::ui_style::modal_backdrop_alpha(dark)),
                );
            });

        let today = chrono::Local::now().date_naive();
        let period_range = self.key_heatmap_page.period.range(today);
        let period_label = match self.key_heatmap_page.period {
            KeyHeatmapPeriod::Custom(range) => custom_period_label(range),
            period => crate::i18n::tr_catalog(lang, period.preset_label_key()).to_string(),
        };
        let mut open = true;
        let mut action: Option<Option<DateRange>> = None;
        let mut cancel = false;
        crate::ui_style::centered_modal_window(
            ctx,
            crate::i18n::tr_catalog(lang, "key_heatmap.clear_title"),
            egui::Id::new("key_heatmap_clear_window"),
            &mut open,
            Vec2::new(460.0, 190.0),
        )
        .show(ctx, |ui| {
            ui.set_min_size(Vec2::new(440.0, 150.0));
            ui.vertical_centered(|ui| {
                ui.add_space(14.0);
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.clear_body"))
                        .size(13.0),
                );
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    let button = Vec2::new(136.0, 30.0);
                    ui.add_space((ui.available_width() - button.x * 3.0 - 20.0).max(0.0) * 0.5);
                    let clear_period = crate::i18n::tr_catalog_format(
                        lang,
                        "key_heatmap.clear_period",
                        &[("period", &period_label)],
                    );
                    // "All time" as a period already means everything.
                    if crate::ui_style::modern_button(
                        ui,
                        &clear_period,
                        button,
                        period_range.is_some(),
                    )
                    .clicked()
                    {
                        action = Some(period_range);
                    }
                    ui.add_space(10.0);
                    if crate::ui_style::modern_button(
                        ui,
                        crate::i18n::tr_catalog(lang, "key_heatmap.clear_all"),
                        button,
                        true,
                    )
                    .clicked()
                    {
                        action = Some(None);
                    }
                    ui.add_space(10.0);
                    if crate::ui_style::modern_button(
                        ui,
                        crate::i18n::tr_catalog(lang, "key_heatmap.cancel"),
                        button,
                        true,
                    )
                    .clicked()
                    {
                        cancel = true;
                    }
                });
            });
        });

        if let Some(range) = action {
            self.key_stats.store.clear(range);
            self.flush_key_stats();
            self.key_heatmap_page.clear_dialog_open = false;
        } else {
            self.key_heatmap_page.clear_dialog_open = open && !cancel;
        }
    }

    fn draw_key_heatmap_board(
        &mut self,
        ui: &mut egui::Ui,
        layout: &KeyboardLayout,
        board_rect: egui::Rect,
        table_rect: egui::Rect,
        dark: bool,
    ) {
        let lang = self.app_settings.language;
        let painter = ui.painter().clone();
        let key_visible = |key: &crate::keyboard::PhysicalKey| {
            Self::layout_key_visible(
                &self.module_settings,
                layout,
                key,
                self.layout_options_value,
            )
        };
        let geometry = layout_geometry_with_reserved_and_filter(
            ui.ctx(),
            layout,
            board_rect,
            clamp_ui_scale(self.app_settings.ui_scale),
            0.0,
            0.0,
            LAYOUT_FIT_MARGIN,
            None,
            key_visible,
            |_| false,
        );

        let today = chrono::Local::now().date_naive();
        let range = self.key_heatmap_page.period.range(today);
        let layer_filter = self
            .key_heatmap_page
            .layer
            .and_then(|layer| u8::try_from(layer).ok());
        let counts = self.key_stats.store.presses(range, layer_filter);
        let total: u64 = counts.values().map(|count| *count as u64).sum();
        let max_count = counts.values().copied().max().unwrap_or(0);
        let log_scale = self.app_settings.key_heatmap.log_scale;

        let visible_keys: Vec<(usize, &crate::keyboard::PhysicalKey)> = layout
            .keys
            .iter()
            .enumerate()
            .filter(|(_, key)| key_visible(key))
            .collect();
        let halves = split_halves(
            &visible_keys
                .iter()
                .map(|(_, key)| (*key).clone())
                .collect::<Vec<_>>(),
        );
        let key_rects: Vec<egui::Rect> = visible_keys
            .iter()
            .map(|(_, key)| layout_physical_key_rect(key, geometry))
            .collect();
        let matrix_idx = |key: &crate::keyboard::PhysicalKey| {
            (key.row as usize * layout.cols + key.col as usize) as u16
        };
        let sources: Vec<HeatSource> = visible_keys
            .iter()
            .zip(&key_rects)
            .zip(&halves)
            .map(|(((_, key), rect), half)| HeatSource {
                center: rect.center(),
                half: *half,
                weight: heat_weight(
                    counts.get(&matrix_idx(key)).copied().unwrap_or(0),
                    max_count,
                    log_scale,
                ),
            })
            .collect();

        let key_rect_of = |idx: u16| {
            visible_keys
                .iter()
                .zip(&key_rects)
                .find(|((_, key), _)| matrix_idx(key) == idx)
                .map(|(_, rect)| *rect)
        };
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.key_heatmap_page.selected_key = None;
        }
        let selected = self
            .key_heatmap_page
            .selected_key
            .filter(|idx| key_rect_of(*idx).is_some());
        self.key_heatmap_page.selected_key = selected;
        let direction = self.key_heatmap_page.direction;
        let routes = selected
            .map(|idx| {
                self.key_stats
                    .store
                    .transitions(range, layer_filter, idx, direction)
            })
            .unwrap_or_default();

        // Heat texture, rebuilt only when the picture would change.
        let heat_area = board_rect.intersect(ui.clip_rect());
        let texture_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for source in &sources {
                source.center.x.to_bits().hash(&mut hasher);
                source.center.y.to_bits().hash(&mut hasher);
                source.half.hash(&mut hasher);
                source.weight.to_bits().hash(&mut hasher);
            }
            heat_area.min.x.to_bits().hash(&mut hasher);
            heat_area.min.y.to_bits().hash(&mut hasher);
            heat_area.max.x.to_bits().hash(&mut hasher);
            heat_area.max.y.to_bits().hash(&mut hasher);
            hasher.finish()
        };
        let now = std::time::Instant::now();
        let stale = self
            .key_heatmap_page
            .heat_texture
            .as_ref()
            .is_none_or(|(key, _)| *key != texture_key);
        let throttled = self
            .key_heatmap_page
            .heat_built_at
            .is_some_and(|built| now.duration_since(built) < HEAT_REBUILD_INTERVAL);
        if stale && throttled && self.key_heatmap_page.heat_texture.is_some() {
            ui.ctx().request_repaint_after(HEAT_REBUILD_INTERVAL);
        } else if stale {
            let image = render_heat_image(heat_area, geometry.unit, &sources);
            let texture =
                ui.ctx()
                    .load_texture("key_heatmap_heat", image, egui::TextureOptions::LINEAR);
            self.key_heatmap_page.heat_texture = Some((texture_key, texture));
            self.key_heatmap_page.heat_built_at = Some(now);
        }
        if let Some((_, texture)) = &self.key_heatmap_page.heat_texture {
            painter.image(
                texture.id(),
                heat_area,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                if selected.is_some() {
                    Color32::from_gray(HEAT_DIM_TINT)
                } else {
                    Color32::WHITE
                },
            );
        }

        // Caps: outlines, faint legends of the shown layer, counts on hover.
        let label_layer = self.key_heatmap_page.layer.unwrap_or(0);
        let outline = if dark {
            Color32::from_white_alpha(46)
        } else {
            Color32::from_black_alpha(54)
        };
        let unused_outline = Color32::from_rgb(86, 170, 255);
        let mut clicked_key = None;
        for (((key_idx, key), rect), source) in visible_keys.iter().zip(&key_rects).zip(&sources) {
            let count = counts.get(&matrix_idx(key)).copied().unwrap_or(0);
            let stroke = if selected == Some(matrix_idx(key)) {
                Stroke::new(2.4_f32, ROUTE_COLOR)
            } else if count == 0 {
                Stroke::new(1.4_f32, unused_outline)
            } else {
                Stroke::new(1.0_f32, outline)
            };
            paint_layout_keycap(&painter, *rect, key.rotation, Color32::TRANSPARENT, stroke);

            let keycode = layout_indicator::layout_effective_keycode(layout, label_layer, *key_idx);
            if keycode != 0 {
                let label = keycode_label_with_macro_names(
                    keycode,
                    &layout.custom_keycodes,
                    &self.layer_names,
                    &self.keycode_picker.macro_names,
                    &self.keycode_picker.tap_dance_names,
                    self.app_settings.key_legend_layout,
                )
                .replace('\n', " ");
                let hot = source.weight > 0.7;
                let color = if hot {
                    Color32::from_black_alpha(150)
                } else {
                    Color32::from_white_alpha(150)
                };
                let font_size = (geometry.unit * 0.2).clamp(8.0, 13.0);
                paint_centered_text_rotated(
                    &painter.with_clip_rect(rect.shrink(3.0)),
                    rect.center(),
                    &label,
                    FontId::proportional(font_size),
                    color,
                    key.rotation.to_radians(),
                );
            }
            let response = ui.interact(
                *rect,
                ui.id().with(("key_heatmap_key", *key_idx)),
                Sense::click(),
            );
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if response.clicked() {
                clicked_key = Some(matrix_idx(key));
            }
            let share = if total > 0 {
                count as f64 * 100.0 / total as f64
            } else {
                0.0
            };
            response.on_hover_text(crate::i18n::tr_catalog_format(
                lang,
                "key_heatmap.presses_tooltip",
                &[
                    ("count", &count.to_string()),
                    ("share", &format!("{share:.1}")),
                ],
            ));
        }
        if let Some(idx) = clicked_key {
            self.key_heatmap_page.selected_key = (selected != Some(idx)).then_some(idx);
        }

        // Routes of the selected key: arrows to (or from) the busiest keys.
        let top = self.app_settings.key_heatmap.top_transitions;
        if let Some(selected_rect) = selected.and_then(key_rect_of) {
            let arrows = physical_routes(&routes, top);
            let max_route = arrows.first().map_or(0, |(_, count)| *count);
            let key_radius = geometry.unit * 0.42;
            for (other, count) in arrows.iter().rev() {
                let width = route_width(*count, max_route);
                if Some(*other) == selected {
                    painter.circle_stroke(
                        selected_rect.center(),
                        key_radius + width * 0.5 + 2.0,
                        Stroke::new(width, ROUTE_COLOR),
                    );
                    continue;
                }
                let Some(other_rect) = key_rect_of(*other) else {
                    continue;
                };
                let (from, to) = match direction {
                    TransitionDirection::Outgoing => (selected_rect.center(), other_rect.center()),
                    TransitionDirection::Incoming => (other_rect.center(), selected_rect.center()),
                };
                paint_route_arrow(&painter, from, to, width, key_radius);
            }
        }
        self.draw_key_heatmap_routes_table(ui, layout, table_rect, selected, &routes, dark);

        if total == 0 {
            painter.text(
                egui::pos2(board_rect.center().x, board_rect.bottom() - 6.0),
                egui::Align2::CENTER_BOTTOM,
                crate::i18n::tr_catalog(lang, "key_heatmap.empty"),
                FontId::proportional(13.0),
                app_muted_text(dark),
            );
        }
    }

    /// Legend of a key on `layer`, falling back to its matrix position.
    fn key_heatmap_key_label(
        &self,
        layout: &KeyboardLayout,
        layer: usize,
        matrix_idx: u16,
    ) -> String {
        let Some((key_idx, key)) = layout.keys.iter().enumerate().find(|(_, key)| {
            key.row as usize * layout.cols + key.col as usize == matrix_idx as usize
        }) else {
            return format!("#{matrix_idx}");
        };
        let keycode = layout_indicator::layout_effective_keycode(layout, layer, key_idx);
        let label = keycode_label_with_macro_names(
            keycode,
            &layout.custom_keycodes,
            &self.layer_names,
            &self.keycode_picker.macro_names,
            &self.keycode_picker.tap_dance_names,
            self.app_settings.key_legend_layout,
        )
        .replace('\n', " ");
        if label.trim().is_empty() {
            format!("r{}c{}", key.row, key.col)
        } else {
            label
        }
    }

    fn draw_key_heatmap_routes_table(
        &mut self,
        ui: &mut egui::Ui,
        layout: &KeyboardLayout,
        rect: egui::Rect,
        selected: Option<u16>,
        routes: &[TransitionCount],
        dark: bool,
    ) {
        let lang = self.app_settings.language;
        let painter = ui.painter().clone();
        let Some(selected) = selected else {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                crate::i18n::tr_catalog(lang, "key_heatmap.routes_hint"),
                FontId::proportional(12.5),
                app_muted_text(dark),
            );
            return;
        };

        let selected_layer = self.key_heatmap_page.layer.unwrap_or(0);
        let title = crate::i18n::tr_catalog_format(
            lang,
            "key_heatmap.routes_title",
            &[(
                "key",
                &self.key_heatmap_key_label(layout, selected_layer, selected),
            )],
        );
        let direction_labels = vec![
            crate::i18n::tr_catalog(lang, "key_heatmap.direction_outgoing").to_string(),
            crate::i18n::tr_catalog(lang, "key_heatmap.direction_incoming").to_string(),
        ];
        let top_labels: Vec<String> = TOP_TRANSITION_CHOICES
            .iter()
            .map(|top| match top {
                0 => crate::i18n::tr_catalog(lang, "key_heatmap.top_all").to_string(),
                top => top.to_string(),
            })
            .collect();
        let mut settings_changed = false;
        crate::ui_style::allocate_ui_at_rect(ui, rect, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(13.0).strong());
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let selected_direction =
                    usize::from(self.key_heatmap_page.direction == TransitionDirection::Incoming);
                if let Some(picked) = crate::ui_style::settings_segmented_control(
                    ui,
                    "key_heatmap_direction",
                    &direction_labels,
                    selected_direction,
                    Vec2::new(150.0, 24.0),
                ) {
                    self.key_heatmap_page.direction = if picked == 0 {
                        TransitionDirection::Outgoing
                    } else {
                        TransitionDirection::Incoming
                    };
                }
                let top = self.app_settings.key_heatmap.top_transitions;
                let selected_top = TOP_TRANSITION_CHOICES
                    .iter()
                    .position(|choice| *choice == top)
                    .unwrap_or(1);
                if let Some(picked) = crate::ui_style::settings_segmented_control(
                    ui,
                    "key_heatmap_top",
                    &top_labels,
                    selected_top,
                    Vec2::new(140.0, 24.0),
                ) {
                    self.app_settings.key_heatmap.top_transitions = TOP_TRANSITION_CHOICES[picked];
                    settings_changed = true;
                }
            });
            ui.add_space(4.0);

            let total: u64 = routes.iter().map(|route| route.count as u64).sum();
            let top = self.app_settings.key_heatmap.top_transitions;
            let shown = if top == 0 {
                routes.len()
            } else {
                routes.len().min(top as usize)
            };
            if routes.is_empty() {
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.routes_empty"))
                        .size(12.0)
                        .color(app_muted_text(dark)),
                );
            }
            egui::ScrollArea::vertical()
                .max_height((rect.bottom() - ui.cursor().top() - 26.0).max(24.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("key_heatmap_routes")
                        .num_columns(3)
                        .spacing(Vec2::new(14.0, 2.0))
                        .show(ui, |ui| {
                            let arrow = match self.key_heatmap_page.direction {
                                TransitionDirection::Outgoing => "→",
                                TransitionDirection::Incoming => "←",
                            };
                            for route in &routes[..shown] {
                                let KeyPos { layer, matrix_idx } = route.other;
                                let label =
                                    self.key_heatmap_key_label(layout, layer as usize, matrix_idx);
                                let layer_text = crate::i18n::tr_catalog_format(
                                    lang,
                                    "key_heatmap.route_layer",
                                    &[("layer", &layer.to_string())],
                                );
                                ui.label(RichText::new(format!("{arrow} {label}")).size(12.5));
                                ui.label(
                                    RichText::new(layer_text)
                                        .size(11.5)
                                        .color(app_muted_text(dark)),
                                );
                                let share = route.count as f64 * 100.0 / total.max(1) as f64;
                                ui.label(
                                    RichText::new(format!("{} · {share:.0}%", route.count))
                                        .size(12.0),
                                );
                                ui.end_row();
                            }
                        });
                });

            ui.horizontal(|ui| {
                let mut show_held = self.app_settings.key_heatmap.show_held_keys_in_routes;
                crate::ui_style::settings_switch(ui, &mut show_held);
                ui.label(
                    RichText::new(crate::i18n::tr_catalog(lang, "key_heatmap.show_held"))
                        .size(11.5)
                        .color(app_muted_text(dark)),
                )
                .on_hover_text(crate::i18n::tr_catalog(
                    lang,
                    "key_heatmap.show_held_tooltip",
                ));
                if show_held != self.app_settings.key_heatmap.show_held_keys_in_routes {
                    self.app_settings.key_heatmap.show_held_keys_in_routes = show_held;
                    settings_changed = true;
                }
            });
        });
        if settings_changed {
            save_app_settings(&self.app_settings);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ironbow_runs_from_dark_to_white_hot() {
        assert_eq!(ironbow(0.0), Color32::from_rgb(8, 6, 30));
        assert_eq!(ironbow(1.0), Color32::from_rgb(255, 252, 235));
        assert_eq!(ironbow(-1.0), ironbow(0.0));
        assert_eq!(ironbow(2.0), ironbow(1.0));
        let brightness = |c: Color32| c.r() as u32 + c.g() as u32 + c.b() as u32;
        let samples: Vec<u32> = (0..=10)
            .map(|i| brightness(ironbow(i as f32 / 10.0)))
            .collect();
        assert!(
            samples.windows(2).all(|pair| pair[0] <= pair[1]),
            "{samples:?}"
        );
    }

    #[test]
    fn periods_use_monday_based_weeks() {
        // 2026-10-02 is a Friday.
        let friday = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let date = |day| chrono::NaiveDate::from_ymd_opt(2026, 9, day).unwrap();
        assert_eq!(
            KeyHeatmapPeriod::ThisWeek.range(friday),
            Some(DateRange::new(date(28), friday))
        );
        assert_eq!(
            KeyHeatmapPeriod::LastWeek.range(friday),
            Some(DateRange::new(date(21), date(27)))
        );
        assert_eq!(KeyHeatmapPeriod::AllTime.range(friday), None);
    }

    #[test]
    fn physical_routes_merge_layers_and_keep_the_busiest() {
        let route = |layer, idx, count| TransitionCount {
            other: KeyPos::new(layer, idx),
            count,
        };
        let routes = [
            route(0, 7, 5),
            route(1, 7, 4),
            route(0, 3, 6),
            route(0, 9, 1),
        ];
        assert_eq!(physical_routes(&routes, 2), vec![(7, 9), (3, 6)]);
        assert_eq!(physical_routes(&routes, 0).len(), 3);
    }

    #[test]
    fn route_width_scales_with_share() {
        assert_eq!(route_width(10, 10), ROUTE_MAX_WIDTH);
        assert_eq!(route_width(0, 10), ROUTE_MIN_WIDTH);
        assert_eq!(route_width(5, 0), ROUTE_MIN_WIDTH);
    }

    #[test]
    fn calendar_covers_whole_weeks_ending_this_week() {
        let friday = chrono::NaiveDate::from_ymd_opt(2026, 10, 2).unwrap();
        let first = calendar_first_day(friday);
        assert_eq!(first.weekday(), chrono::Weekday::Mon);
        assert_eq!((friday - first).num_days(), 52 * 7 + 4);
    }

    #[test]
    fn calendar_shift_click_extends_from_the_anchor() {
        let day = |d| chrono::NaiveDate::from_ymd_opt(2026, 10, d).unwrap();
        assert_eq!(
            calendar_click_range(Some(day(1)), day(5), false),
            DateRange::new(day(5), day(5))
        );
        assert_eq!(
            calendar_click_range(Some(day(5)), day(1), true),
            DateRange::new(day(1), day(5))
        );
        assert_eq!(
            calendar_click_range(None, day(3), true),
            DateRange::new(day(3), day(3))
        );
        assert_eq!(
            custom_period_label(DateRange::new(day(1), day(5))),
            "01.10–05.10"
        );
    }

    #[test]
    fn log_scale_lifts_rare_keys() {
        assert_eq!(heat_weight(0, 100, true), 0.0);
        assert_eq!(heat_weight(100, 100, true), 1.0);
        assert!(heat_weight(10, 1000, true) > 4.0 * heat_weight(10, 1000, false));
    }

    #[test]
    fn heat_stays_on_its_half() {
        let unit = 40.0;
        let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 40.0));
        let sources = [
            HeatSource {
                center: egui::pos2(20.0, 20.0),
                half: 0,
                weight: 1.0,
            },
            // Right-half neighbor close enough for the left key's heat to reach.
            HeatSource {
                center: egui::pos2(60.0, 20.0),
                half: 1,
                weight: 0.0,
            },
        ];
        let image = render_heat_image(area, unit, &sources);
        let pixel = |x: f32| {
            image.pixels
                [(10.0 / HEAT_PIXEL_SIZE) as usize * image.width() + (x / HEAT_PIXEL_SIZE) as usize]
        };
        assert_eq!(pixel(20.0).a(), 255);
        assert!(pixel(20.0).r() > 200, "hot key center");
        // At the right key's center: still opaque, but cold.
        assert_eq!(pixel(60.0), ironbow(0.0));
        // Far from every key: transparent.
        assert_eq!(pixel(300.0).a(), 0);
    }
}
