//! Pure layout, rendering, and motion planning for noninteractive Toasts.
//!
//! This module has no terminal or tmux IO. It turns normalized content and a
//! per-window viewport into immutable plans that the tmux boundary can apply in
//! a batch. Notification text is never interpreted as ANSI or as a command.

use std::cmp::Reverse;
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::config::{BodyPresentation, Easing, Placement, StackOrder};
use crate::notification::{Level, Timeout};

pub const MIN_TOAST_WIDTH: u16 = 24;
pub const DEFAULT_TOAST_WIDTH: u16 = 42;
pub const DEFAULT_TOAST_HEIGHT: u16 = 3;
pub const MAX_ANIMATION_FPS: u32 = 120;
pub const MAX_ANIMATION_DURATION: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Surface {
    Bordered,
    Borderless,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlyphMode {
    Unicode,
    Ascii,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    Ansi16,
    Monochrome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayAge {
    Finite { remaining: Duration },
    Never,
}

impl DisplayAge {
    #[must_use]
    pub fn from_timeout(timeout: Timeout, remaining: Option<Duration>) -> Self {
        match timeout {
            Timeout::After(duration) => Self::Finite {
                remaining: remaining.unwrap_or(duration),
            },
            Timeout::Never => Self::Never,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Toast<'a> {
    /// Stable FIFO rank assigned by the scheduler.
    pub sequence: u64,
    pub level: Level,
    pub title: &'a str,
    pub body: &'a str,
    pub notification_key: Option<&'a str>,
    pub age: DisplayAge,
    /// Distinguishes an in-place keyed update without replaying enter motion.
    pub updated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LayoutOptions {
    pub placement: Placement,
    pub width: u16,
    pub height: u16,
    pub max_visible: usize,
    pub gap: u16,
    pub stack_order: StackOrder,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            placement: Placement::TopRight,
            width: DEFAULT_TOAST_WIDTH,
            height: DEFAULT_TOAST_HEIGHT,
            max_visible: 4,
            gap: 1,
            stack_order: StackOrder::OldestFirst,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Slot<'a> {
    Toast(&'a Toast<'a>),
    Waiting(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlacedSlot<'a> {
    pub rect: Rect,
    pub surface: Surface,
    pub slot: Slot<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowLayout<'a> {
    pub slots: Vec<PlacedSlot<'a>>,
    pub suppressed: usize,
}

/// Computes capacity independently for one Attention Window. Overflow occupies
/// the innermost slot so `+N waiting` is truthful and never overlaps a Toast.
#[must_use]
pub fn layout_window<'a>(
    viewport: Viewport,
    toasts: &'a [Toast<'a>],
    options: LayoutOptions,
) -> WindowLayout<'a> {
    if viewport.width < MIN_TOAST_WIDTH || options.max_visible == 0 {
        return WindowLayout {
            slots: Vec::new(),
            suppressed: toasts.len(),
        };
    }

    let surface = if viewport.width >= options.width {
        Surface::Bordered
    } else {
        Surface::Borderless
    };
    let (slot_width, slot_height) = match surface {
        Surface::Bordered => (options.width, options.height),
        Surface::Borderless => (viewport.width, 1),
    };
    let per_window = usize::from(
        viewport.height.saturating_add(options.gap)
            / slot_height.saturating_add(options.gap).max(1),
    )
    .min(options.max_visible);
    if per_window == 0 {
        return WindowLayout {
            slots: Vec::new(),
            suppressed: toasts.len(),
        };
    }

    let mut ordered: Vec<&Toast<'_>> = toasts.iter().collect();
    match options.stack_order {
        StackOrder::OldestFirst => ordered.sort_by_key(|toast| toast.sequence),
        StackOrder::NewestFirst => ordered.sort_by_key(|toast| Reverse(toast.sequence)),
    }

    let toast_capacity = if ordered.len() > per_window {
        per_window.saturating_sub(1)
    } else {
        per_window
    };
    let waiting = ordered.len().saturating_sub(toast_capacity);
    let mut values: Vec<Slot<'_>> = ordered
        .into_iter()
        .take(toast_capacity)
        .map(Slot::Toast)
        .collect();
    if waiting > 0 {
        values.push(Slot::Waiting(waiting));
    }

    let x = horizontal_origin(viewport.width, slot_width, options.placement);
    let step = slot_height.saturating_add(options.gap);
    let bottom = matches!(
        options.placement,
        Placement::BottomLeft | Placement::BottomCenter | Placement::BottomRight
    );
    let slots = values
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            let offset = u16::try_from(index)
                .unwrap_or(u16::MAX)
                .saturating_mul(step);
            let y = if bottom {
                viewport
                    .height
                    .saturating_sub(slot_height)
                    .saturating_sub(offset)
            } else {
                offset
            };
            PlacedSlot {
                rect: Rect {
                    x,
                    y,
                    width: slot_width,
                    height: slot_height,
                },
                surface,
                slot,
            }
        })
        .collect();

    WindowLayout {
        slots,
        suppressed: waiting,
    }
}

fn horizontal_origin(window_width: u16, width: u16, placement: Placement) -> u16 {
    match placement {
        Placement::TopLeft | Placement::BottomLeft => 0,
        Placement::TopCenter | Placement::BottomCenter => window_width.saturating_sub(width) / 2,
        Placement::TopRight | Placement::BottomRight => window_width.saturating_sub(width),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderOptions {
    pub body: BodyPresentation,
    pub glyphs: GlyphMode,
    pub color: ColorMode,
    /// The hint is informational. Toast renderers never read input.
    pub show_jump_hint: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            body: BodyPresentation::FirstLine,
            glyphs: GlyphMode::Unicode,
            color: ColorMode::Ansi16,
            show_jump_hint: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedToast {
    /// Plain cell rows; useful for snapshots and monochrome output.
    pub lines: Vec<String>,
    /// Same rows with semantic ANSI16 styling. User text is always unstyled.
    pub styled_lines: Vec<String>,
    /// Kept explicit so an IO adapter cannot accidentally capture focus/input.
    pub interactive: bool,
}

#[must_use]
pub fn render_toast(
    toast: &Toast<'_>,
    rect: Rect,
    surface: Surface,
    options: RenderOptions,
) -> RenderedToast {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return RenderedToast {
            lines: Vec::new(),
            styled_lines: Vec::new(),
            interactive: false,
        };
    }

    let title = safe_plain_text(toast.title);
    let body = safe_plain_text(toast.body);
    let (symbol, label) = level_identity(toast.level, options.glyphs);
    let status = status_text(toast);
    let hint = toast
        .notification_key
        .filter(|_| options.show_jump_hint)
        .map(|key| format!("jump: tmnotify jump --key {}", safe_plain_text(key)));

    let lines = match surface {
        Surface::Borderless => {
            let content = format!("{symbol} {label} · {} — {}", title, compact_body(&body));
            vec![fit_line(&content, width, true)]
        }
        Surface::Bordered => render_bordered(
            &title,
            &body,
            width,
            height,
            options.body,
            options.glyphs,
            symbol,
            label,
            &status,
            hint.as_deref(),
        ),
    };
    let styled_lines = lines
        .iter()
        .map(|line| style_line(line, toast.level, options.color))
        .collect();
    RenderedToast {
        lines,
        styled_lines,
        interactive: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bordered(
    title: &str,
    body: &str,
    width: usize,
    height: usize,
    body_mode: BodyPresentation,
    glyphs: GlyphMode,
    symbol: &str,
    label: &str,
    status: &str,
    hint: Option<&str>,
) -> Vec<String> {
    if width < 2 || height < 2 {
        return vec![fit_line(title, width, true); height];
    }
    let (tl, tr, bl, br, horizontal, vertical) = match glyphs {
        GlyphMode::Unicode => ("┌", "┐", "└", "┘", "─", "│"),
        GlyphMode::Ascii => ("+", "+", "+", "+", "-", "|"),
    };
    let inner_width = width - 2;
    let inner_height = height - 2;
    let mut result = Vec::with_capacity(height);
    result.push(format!("{tl}{}{tr}", horizontal.repeat(inner_width)));

    if inner_height == 1 {
        let body = match body_mode {
            BodyPresentation::FirstLine | BodyPresentation::Wrap => compact_body(body),
            BodyPresentation::JoinLines => body
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        };
        let content = format!("{symbol} {label} · {status} · {title} — {body}");
        result.push(format!(
            "{vertical}{}{vertical}",
            fit_line(&content, inner_width, false)
        ));
        result.push(format!("{bl}{}{br}", horizontal.repeat(inner_width)));
        return result;
    }

    let heading = format!("{symbol} {label} · {title} · {status}");
    let body_rows = body_lines(body, body_mode, inner_width, inner_height);
    let mut content = vec![heading];
    content.extend(body_rows);
    if let Some(hint) = hint {
        content.push(hint.to_owned());
    }
    for row in 0..inner_height {
        let value = content.get(row).map(String::as_str).unwrap_or("");
        result.push(format!(
            "{vertical}{}{vertical}",
            fit_line(value, inner_width, false)
        ));
    }
    result.push(format!("{bl}{}{br}", horizontal.repeat(inner_width)));
    result
}

fn body_lines(body: &str, mode: BodyPresentation, width: usize, rows: usize) -> Vec<String> {
    if rows <= 1 || width == 0 {
        return Vec::new();
    }
    match mode {
        BodyPresentation::FirstLine => {
            let nonempty: Vec<&str> = body
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            nonempty.first().map_or_else(Vec::new, |line| {
                vec![fit_line(line.trim(), width, nonempty.len() > 1)]
            })
        }
        BodyPresentation::JoinLines => {
            let joined = body
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            if joined.is_empty() {
                Vec::new()
            } else {
                vec![fit_line(&joined, width, false)]
            }
        }
        BodyPresentation::Wrap => wrap_lines(body, width, rows - 1),
    }
}

fn wrap_lines(value: &str, width: usize, maximum: usize) -> Vec<String> {
    if width == 0 || maximum == 0 {
        return Vec::new();
    }
    let words = value.split_whitespace();
    let mut lines: Vec<String> = Vec::new();
    for word in words {
        if lines.is_empty() {
            lines.push(fit_line(word, width, false).trim_end().to_owned());
            continue;
        }
        let needs_space = !lines.last().is_some_and(String::is_empty);
        let candidate_width = lines
            .last()
            .map_or(0, |line| UnicodeWidthStr::width(line.as_str()))
            + usize::from(needs_space)
            + UnicodeWidthStr::width(word);
        if candidate_width <= width {
            let line = lines.last_mut().expect("line was inserted");
            if needs_space {
                line.push(' ');
            }
            line.push_str(word);
        } else if lines.len() < maximum {
            lines.push(fit_line(word, width, false).trim_end().to_owned());
        } else {
            let last = lines.last_mut().expect("at least one line");
            *last = fit_line(last, width, true).trim_end().to_owned();
            break;
        }
    }
    lines
}

fn safe_plain_text(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| match character {
            '\u{1b}' | '\u{7f}'..='\u{9f}' => None,
            '\n' => Some('\n'),
            '\t' => Some(' '),
            character if character.is_control() => Some('�'),
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => Some('�'),
            character => Some(character),
        })
        .collect()
}

#[must_use]
pub fn render_waiting(
    count: usize,
    rect: Rect,
    surface: Surface,
    glyphs: GlyphMode,
    color: ColorMode,
) -> RenderedToast {
    let marker = match glyphs {
        GlyphMode::Unicode => "…",
        GlyphMode::Ascii => "+",
    };
    let content = format!("{marker} +{count} waiting");
    let width = usize::from(rect.width);
    let lines = match surface {
        Surface::Borderless => vec![fit_line(&content, width, false)],
        Surface::Bordered => {
            let (tl, tr, bl, br, horizontal, vertical) = match glyphs {
                GlyphMode::Unicode => ("┌", "┐", "└", "┘", "─", "│"),
                GlyphMode::Ascii => ("+", "+", "+", "+", "-", "|"),
            };
            let inner = width.saturating_sub(2);
            let mut rows = vec![format!("{tl}{}{tr}", horizontal.repeat(inner))];
            for row in 0..usize::from(rect.height.saturating_sub(2)) {
                let value = if row == 0 { content.as_str() } else { "" };
                rows.push(format!(
                    "{vertical}{}{vertical}",
                    fit_line(value, inner, false)
                ));
            }
            rows.push(format!("{bl}{}{br}", horizontal.repeat(inner)));
            rows
        }
    };
    let styled_lines = lines
        .iter()
        .map(|line| style_line(line, Level::Info, color))
        .collect();
    RenderedToast {
        lines,
        styled_lines,
        interactive: false,
    }
}

fn compact_body(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_owned()
}

fn fit_line(value: &str, width: usize, force_ellipsis: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let current = UnicodeWidthStr::width(value);
    if current <= width && !force_ellipsis {
        return format!("{value}{}", " ".repeat(width - current));
    }
    let ellipsis = "…";
    let target = width.saturating_sub(UnicodeWidthStr::width(ellipsis));
    let mut output = String::new();
    let mut used = 0;
    for grapheme in value.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if used + cells > target {
            break;
        }
        output.push_str(grapheme);
        used += cells;
    }
    output.push_str(ellipsis);
    used += 1;
    output.push_str(&" ".repeat(width.saturating_sub(used)));
    output
}

fn level_identity(level: Level, glyphs: GlyphMode) -> (&'static str, &'static str) {
    match (level, glyphs) {
        (Level::Info, GlyphMode::Unicode) => ("●", "info"),
        (Level::Success, GlyphMode::Unicode) => ("✓", "ok"),
        (Level::Warning, GlyphMode::Unicode) => ("▲", "warning"),
        (Level::Error, GlyphMode::Unicode) => ("✕", "error"),
        (Level::Info, GlyphMode::Ascii) => ("[i]", "info"),
        (Level::Success, GlyphMode::Ascii) => ("[ok]", "ok"),
        (Level::Warning, GlyphMode::Ascii) => ("[!]", "warning"),
        (Level::Error, GlyphMode::Ascii) => ("[x]", "error"),
    }
}

fn status_text(toast: &Toast<'_>) -> String {
    let base = match toast.age {
        DisplayAge::Never => "persistent".to_owned(),
        DisplayAge::Finite { remaining } => {
            let millis = remaining.as_millis();
            if millis < 1_000 {
                format!("{millis}ms")
            } else {
                format!("{}s", remaining.as_secs())
            }
        }
    };
    if toast.updated {
        format!("updated · {base}")
    } else {
        base
    }
}

fn style_line(line: &str, level: Level, color: ColorMode) -> String {
    if color == ColorMode::Monochrome {
        return line.to_owned();
    }
    let code = match level {
        Level::Info => 36,
        Level::Success => 32,
        Level::Warning => 33,
        Level::Error => 31,
    };
    format!("\u{1b}[{code}m{line}\u{1b}[0m")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MotionPhase {
    Enter,
    Stay,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionTrack<'a> {
    pub window_id: &'a str,
    pub from: Rect,
    pub to: Rect,
    pub phase: MotionPhase,
    pub duration: Duration,
    pub easing: Easing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameUpdate<'a> {
    pub window_id: &'a str,
    pub rect: Rect,
    pub phase: MotionPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameBatch<'a> {
    pub at: Duration,
    pub updates: Vec<FrameUpdate<'a>>,
}

/// Produces one batch per frame across all windows. Cell-quantized duplicate
/// positions are dropped per track; the final position is always emitted.
#[must_use]
pub fn plan_frames<'a>(tracks: &[MotionTrack<'a>], fps: u32, enabled: bool) -> Vec<FrameBatch<'a>> {
    if tracks.is_empty() || fps == 0 {
        return Vec::new();
    }
    let fps = fps.min(MAX_ANIMATION_FPS);
    let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
    let maximum = tracks
        .iter()
        .map(|track| track.duration.min(MAX_ANIMATION_DURATION))
        .max()
        .unwrap_or_default();
    let mut times = vec![Duration::ZERO];
    if enabled && !maximum.is_zero() {
        let mut at = frame;
        while at < maximum {
            times.push(at);
            at = at.saturating_add(frame);
        }
        times.extend(
            tracks
                .iter()
                .map(|track| track.duration.min(MAX_ANIMATION_DURATION)),
        );
        times.sort_unstable();
        times.dedup();
    }
    let mut previous: Vec<Option<Rect>> = vec![None; tracks.len()];
    let mut batches = Vec::new();
    for at in times {
        let mut updates = Vec::new();
        for (index, track) in tracks.iter().enumerate() {
            let duration = track.duration.min(MAX_ANIMATION_DURATION);
            let progress = if !enabled || duration.is_zero() || at >= duration {
                1.0
            } else {
                at.as_secs_f64() / duration.as_secs_f64()
            };
            let eased = ease(progress, track.easing);
            let rect = interpolate(track.from, track.to, eased);
            if previous[index] != Some(rect) {
                updates.push(FrameUpdate {
                    window_id: track.window_id,
                    rect,
                    phase: track.phase,
                });
                previous[index] = Some(rect);
            }
        }
        if !updates.is_empty() {
            batches.push(FrameBatch { at, updates });
        }
    }
    batches
}

#[must_use]
pub fn offscreen_rect(target: Rect, viewport: Viewport, placement: Placement) -> Rect {
    let (x, y) = match placement {
        Placement::TopLeft | Placement::BottomLeft => (
            i32::from(target.x.saturating_add(target.width)),
            i32::from(target.y),
        ),
        Placement::TopRight | Placement::BottomRight => (
            i32::from(target.x.saturating_sub(target.width)),
            i32::from(target.y),
        ),
        Placement::TopCenter => (
            i32::from(target.x),
            i32::from(target.y.saturating_add(target.height)),
        ),
        Placement::BottomCenter => (
            i32::from(target.x),
            i32::from(target.y.saturating_sub(target.height)),
        ),
    };
    Rect {
        x: clamp_cell(x).min(viewport.width.saturating_sub(target.width)),
        y: clamp_cell(y).min(viewport.height.saturating_sub(target.height)),
        ..target
    }
}

fn ease(progress: f64, easing: Easing) -> f64 {
    let progress = progress.clamp(0.0, 1.0);
    match easing {
        Easing::EaseIn => progress * progress,
        Easing::EaseOut => 1.0 - (1.0 - progress) * (1.0 - progress),
    }
}

fn interpolate(from: Rect, to: Rect, progress: f64) -> Rect {
    Rect {
        x: lerp(from.x, to.x, progress),
        y: lerp(from.y, to.y, progress),
        width: to.width,
        height: to.height,
    }
}

fn lerp(from: u16, to: u16, progress: f64) -> u16 {
    let value = f64::from(from) + (f64::from(to) - f64::from(from)) * progress;
    clamp_cell(value.round() as i32)
}

fn clamp_cell(value: i32) -> u16 {
    u16::try_from(value.max(0)).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toast(sequence: u64, title: &'static str) -> Toast<'static> {
        Toast {
            sequence,
            level: Level::Success,
            title,
            body: "一行目 👩‍💻\nsecond line",
            notification_key: Some("build"),
            age: DisplayAge::Never,
            updated: false,
        }
    }

    #[test]
    fn all_six_placements_anchor_and_grow_inward() {
        let toasts = [toast(1, "one"), toast(2, "two")];
        for placement in [
            Placement::TopLeft,
            Placement::TopCenter,
            Placement::TopRight,
            Placement::BottomLeft,
            Placement::BottomCenter,
            Placement::BottomRight,
        ] {
            let layout = layout_window(
                Viewport {
                    width: 100,
                    height: 20,
                },
                &toasts,
                LayoutOptions {
                    placement,
                    ..LayoutOptions::default()
                },
            );
            let expected_x = match placement {
                Placement::TopLeft | Placement::BottomLeft => 0,
                Placement::TopCenter | Placement::BottomCenter => 29,
                Placement::TopRight | Placement::BottomRight => 58,
            };
            assert_eq!(layout.slots[0].rect.x, expected_x);
            if matches!(
                placement,
                Placement::TopLeft | Placement::TopCenter | Placement::TopRight
            ) {
                assert_eq!((layout.slots[0].rect.y, layout.slots[1].rect.y), (0, 4));
            } else {
                assert_eq!((layout.slots[0].rect.y, layout.slots[1].rect.y), (17, 13));
            }
        }
    }

    #[test]
    fn order_capacity_and_waiting_are_per_window() {
        let toasts = [toast(1, "old"), toast(2, "middle"), toast(3, "new")];
        let small = layout_window(
            Viewport {
                width: 41,
                height: 3,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert_eq!(small.slots.len(), 2); // borderless rows with a gap
        assert!(matches!(
            small.slots[0].slot,
            Slot::Toast(Toast { sequence: 1, .. })
        ));
        assert!(matches!(small.slots[1].slot, Slot::Waiting(2)));
        let large = layout_window(
            Viewport {
                width: 100,
                height: 20,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert_eq!(large.slots.len(), 3);
        let newest = layout_window(
            Viewport {
                width: 100,
                height: 20,
            },
            &toasts,
            LayoutOptions {
                stack_order: StackOrder::NewestFirst,
                ..LayoutOptions::default()
            },
        );
        assert!(matches!(
            newest.slots[0].slot,
            Slot::Toast(Toast { sequence: 3, .. })
        ));
    }

    #[test]
    fn narrow_breakpoints_are_truthful() {
        let toasts = [toast(1, "one")];
        let hidden = layout_window(
            Viewport {
                width: 23,
                height: 10,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert!(hidden.slots.is_empty());
        assert_eq!(hidden.suppressed, 1);
        let compact = layout_window(
            Viewport {
                width: 24,
                height: 10,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert_eq!(compact.slots[0].surface, Surface::Borderless);
        assert_eq!(compact.slots[0].rect.height, 1);
        let normal = layout_window(
            Viewport {
                width: 42,
                height: 10,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert_eq!(normal.slots[0].surface, Surface::Bordered);
        assert_eq!(
            (normal.slots[0].rect.width, normal.slots[0].rect.height),
            (42, 3)
        );
        let too_short = layout_window(
            Viewport {
                width: 42,
                height: 2,
            },
            &toasts,
            LayoutOptions::default(),
        );
        assert!(too_short.slots.is_empty());
        assert_eq!(too_short.suppressed, 1);
    }

    #[test]
    fn bordered_snapshot_is_fixed_size_cell_aware_and_noninteractive() {
        let rendered = render_toast(
            &toast(1, "ビルド完了 👩‍💻 with a long title"),
            Rect {
                x: 0,
                y: 0,
                width: 42,
                height: 5,
            },
            Surface::Bordered,
            RenderOptions::default(),
        );
        assert_eq!(
            rendered.lines,
            vec![
                "┌────────────────────────────────────────┐",
                "│✓ ok · ビルド完了 👩‍💻 with a long title …│",
                "│一行目 👩‍💻…                              │",
                "│jump: tmnotify jump --key build         │",
                "└────────────────────────────────────────┘",
            ]
        );
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) == 42)
        );
        assert!(!rendered.interactive);
        assert!(rendered.styled_lines[1].starts_with("\u{1b}[32m"));
    }

    #[test]
    fn default_three_row_toast_keeps_status_title_and_body_visible() {
        let rendered = render_toast(
            &toast(1, "Build"),
            Rect {
                x: 0,
                y: 0,
                width: 42,
                height: 3,
            },
            Surface::Bordered,
            RenderOptions::default(),
        );
        assert!(rendered.lines[1].contains("persistent"));
        assert!(rendered.lines[1].contains("Build"));
        assert!(rendered.lines[1].contains("一行目"));
    }

    #[test]
    fn untrusted_text_cannot_emit_terminal_controls() {
        let mut value = toast(1, "bad\u{1b}[31m title");
        value.body = "body\u{7} text";
        value.notification_key = Some("key\u{1b}[2J");
        let rendered = render_toast(
            &value,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 5,
            },
            Surface::Bordered,
            RenderOptions {
                color: ColorMode::Monochrome,
                ..RenderOptions::default()
            },
        );
        assert!(!rendered.lines.join("").contains('\u{1b}'));
        assert!(rendered.lines.join("").contains("[31m"));
    }

    #[test]
    fn waiting_indicator_has_fixed_geometry_and_fallbacks() {
        let unicode = render_waiting(
            12,
            Rect {
                x: 0,
                y: 0,
                width: 42,
                height: 3,
            },
            Surface::Bordered,
            GlyphMode::Unicode,
            ColorMode::Monochrome,
        );
        assert!(unicode.lines[1].contains("+12 waiting"));
        assert!(
            unicode
                .lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) == 42)
        );
        let ascii = render_waiting(
            2,
            Rect {
                x: 0,
                y: 0,
                width: 24,
                height: 1,
            },
            Surface::Borderless,
            GlyphMode::Ascii,
            ColorMode::Monochrome,
        );
        assert_eq!(ascii.lines, vec!["+ +2 waiting            "]);
    }

    #[test]
    fn ascii_monochrome_and_update_finite_status_have_redundant_signals() {
        let mut value = toast(1, "Done");
        value.age = DisplayAge::Finite {
            remaining: Duration::from_millis(850),
        };
        value.updated = true;
        let rendered = render_toast(
            &value,
            Rect {
                x: 0,
                y: 0,
                width: 42,
                height: 3,
            },
            Surface::Bordered,
            RenderOptions {
                glyphs: GlyphMode::Ascii,
                color: ColorMode::Monochrome,
                ..RenderOptions::default()
            },
        );
        assert!(rendered.lines[1].contains("[ok] ok"));
        assert!(rendered.lines[1].contains("updated"));
        assert_eq!(rendered.lines, rendered.styled_lines);
        assert!(rendered.lines[0].is_ascii());
        assert!(!rendered.lines.iter().any(|line| line.contains('\u{1b}')));
    }

    #[test]
    fn body_modes_handle_more_content_without_growing() {
        let value = toast(1, "T");
        let first = body_lines(value.body, BodyPresentation::FirstLine, 10, 4);
        assert_eq!(first, vec!["一行目 👩‍💻…"]);
        let joined = body_lines(value.body, BodyPresentation::JoinLines, 14, 4);
        assert_eq!(joined, vec!["一行目 👩‍💻 sec…"]);
        let wrapped = body_lines("alpha beta gamma delta", BodyPresentation::Wrap, 10, 3);
        assert_eq!(wrapped, vec!["alpha beta", "gamma…"]);
    }

    #[test]
    fn animation_batches_windows_and_drops_quantized_duplicates() {
        let tracks = [
            MotionTrack {
                window_id: "@1",
                from: Rect {
                    x: 100,
                    y: 0,
                    width: 42,
                    height: 3,
                },
                to: Rect {
                    x: 58,
                    y: 0,
                    width: 42,
                    height: 3,
                },
                phase: MotionPhase::Enter,
                duration: Duration::from_millis(180),
                easing: Easing::EaseOut,
            },
            MotionTrack {
                window_id: "@2",
                from: Rect {
                    x: 80,
                    y: 0,
                    width: 42,
                    height: 3,
                },
                to: Rect {
                    x: 38,
                    y: 0,
                    width: 42,
                    height: 3,
                },
                phase: MotionPhase::Enter,
                duration: Duration::from_millis(180),
                easing: Easing::EaseOut,
            },
        ];
        let frames = plan_frames(&tracks, 20, true);
        assert!(frames.iter().all(|batch| batch.updates.len() == 2));
        assert_eq!(frames.first().unwrap().updates[0].rect.x, 100);
        assert_eq!(frames.last().unwrap().updates[0].rect.x, 58);
        assert!(
            frames
                .windows(2)
                .all(|pair| pair[0].updates[0].rect != pair[1].updates[0].rect)
        );
        let disabled = plan_frames(&tracks, 20, false);
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].updates[0].rect, tracks[0].to);
    }

    #[test]
    fn update_and_stay_are_single_non_enter_frames() {
        let target = Rect {
            x: 4,
            y: 5,
            width: 42,
            height: 3,
        };
        let frames = plan_frames(
            &[MotionTrack {
                window_id: "@1",
                from: target,
                to: target,
                phase: MotionPhase::Stay,
                duration: Duration::ZERO,
                easing: Easing::EaseOut,
            }],
            20,
            true,
        );
        assert_eq!(
            frames,
            vec![FrameBatch {
                at: Duration::ZERO,
                updates: vec![FrameUpdate {
                    window_id: "@1",
                    rect: target,
                    phase: MotionPhase::Stay
                }]
            }]
        );
    }
}
