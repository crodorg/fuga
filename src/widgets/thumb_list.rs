#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;
use std::time::Duration;

use crate::theme::Theme;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;
use tokio::sync::mpsc::UnboundedSender;

use crate::art_cache::ArtCache;
use crate::dispatch::Dispatcher;
use crate::term_probe::ThumbMode;
use crate::types::ArtSize;

/// Substring match (case-insensitive) on a row's user-visible text. Returns
/// true if `q_lower` is found in label, or in any of the column fields when
/// the row uses the `track_cols` layout. Empty `q_lower` matches everything.
pub fn row_matches(row: &ThumbRowSpec, q_lower: &str) -> bool {
    if q_lower.is_empty() {
        return true;
    }
    if row.label.to_lowercase().contains(q_lower) {
        return true;
    }
    if let Some(c) = &row.track_cols {
        if c.title.to_lowercase().contains(q_lower)
            || c.artist.to_lowercase().contains(q_lower)
            || c.album.to_lowercase().contains(q_lower)
        {
            return true;
        }
    }
    false
}

#[derive(Clone)]
pub struct ThumbRowSpec {
    /// Plain label rendered when `track_cols.is_none()`. Track-style rows
    /// ignore this and render via the column layout below; non-track rows
    /// (album / artist / playlist landing pages, section headers, queue
    /// items in non-track sources, etc.) use this single string.
    pub label: String,
    pub art_uri: Option<String>,
    pub source_scheme: Option<&'static str>,
    /// rmpc-style `Artist | Title | Album | Len` columned layout. When `Some`,
    /// the row renders as 4 columns with the last right-aligned. Falls back
    /// to `label` otherwise.
    pub track_cols: Option<TrackColumns>,
    /// Marks the row that is currently playing. Renders the row text in the
    /// accent theme + bold (text-only highlight; the cursor's full-bar
    /// selection style still wins when the row is also under the cursor).
    pub is_now_playing: bool,
    /// Marks the row as user-pinned. Renders a small accent-styled marker
    /// in the left margin so the user can spot pins at a glance. The
    /// pinning order is enforced separately in `apply_pinning`.
    pub pinned: bool,
    /// Renders as a non-selectable group header (theme.header style, no
    /// thumb column, single-cell row). Selection bar still draws if the
    /// cursor lands here — callers either no-op on activate or move past.
    pub is_header: bool,
    /// Forces the row height (in cells) when this row has art and inline
    /// thumbs are enabled. Defaults to the list's `thumb_cells`. Lets one
    /// row use a taller thumbnail than its neighbours (e.g. YT search
    /// rows rendered at 3 cells while everything else stays at 2).
    pub row_h_override: Option<u16>,
}

/// Column data for a track row. Empty strings render as blanks; duration
/// `None` renders as "—".
#[derive(Clone)]
pub struct TrackColumns {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration: Option<Duration>,
}

impl ThumbRowSpec {
    pub fn plain(
        label: impl Into<String>,
        art_uri: Option<String>,
        source_scheme: Option<&'static str>,
    ) -> Self {
        Self {
            label: label.into(),
            art_uri,
            source_scheme,
            track_cols: None,
            is_now_playing: false,
            pinned: false,
            is_header: false,
            row_h_override: None,
        }
    }

}

pub struct ThumbListCtx<'a> {
    pub area: Rect,
    pub title: String,
    pub rows: &'a [ThumbRowSpec],
    pub cursor: usize,
    pub top: &'a mut usize,
    pub thumb_cells: u16,
    pub mode: ThumbMode,
    pub theme: &'a Theme,
    /// Absolute terminal `y` of the album-art panel's TOP border, when one
    /// is rendered. The scrollbar shrinks vertically so it ends just above
    /// this row — otherwise the bar would be hidden behind the art image.
    pub art_top_y: Option<u16>,
    /// Filled by the renderer with the heights (in cells) of the rows that
    /// were drawn, in display order starting from `*top`. Mouse hit-testing
    /// walks this instead of dividing by a single `row_h` (per-row heights
    /// vary when only some rows have art).
    pub visible_row_heights: &'a mut Vec<u16>,
    /// Filled by the renderer with (rect, uri) pairs for every visible
    /// thumbnail cell. The mouse handler walks these to detect clicks on
    /// the thumbnail image so it can expand the cover full-screen.
    pub thumb_hits: &'a mut Vec<(Rect, String)>,
    /// Override for the count rendered in the title. None → use rows.len()
    /// (the default browse behavior). Set this when the row list includes
    /// non-data rows (e.g. search-result group headers) and the count
    /// should reflect only the data rows.
    pub count_override: Option<usize>,
}

pub fn render_thumb_list(
    f: &mut Frame<'_>,
    ctx: ThumbListCtx<'_>,
    art_cache: &Arc<ArtCache>,
    dispatcher: &Dispatcher,
    picker: &Picker,
    protocols: &mut HashMap<String, StatefulProtocol>,
    fetching: &mut HashSet<String>,
    wake: &UnboundedSender<()>,
) {
    let title_count = ctx.count_override.unwrap_or(ctx.rows.len());
    let block = Block::default()
        .title(format!("{} ({})", ctx.title, title_count))
        .borders(Borders::ALL)
        .border_style(ctx.theme.block_border());
    let inner = block.inner(ctx.area);
    f.render_widget(block, ctx.area);

    if inner.width == 0 || inner.height == 0 || ctx.rows.is_empty() {
        return;
    }

    // Per-row variable height: rows with art take `thumb_cells` cells,
    // rows without take 1. Lets a mixed list (e.g. local Albums where
    // some have covers and some don't) avoid a forced 2-cell row for
    // the iconless entries.
    // Sixel can't anchor cells to scrolling rows, so per-row inline thumbs
    // are off in Sixel mode just like in Off mode. Now-playing big art still
    // renders elsewhere as sixel.
    let thumbs_off = !ctx.mode.supports_row_thumbs();
    let big_h = ctx.thumb_cells.max(1);
    let row_h_for = |spec: &ThumbRowSpec| -> u16 {
        if spec.is_header {
            1
        } else if !thumbs_off && spec.art_uri.is_some() {
            spec.row_h_override.unwrap_or(big_h)
        } else {
            1
        }
    };
    // Whether the column should be reserved at all on this list. Same
    // heuristic as before — when no row in this list has art, drop the
    // column entirely so single-line rows don't waste a leading 2 cells.
    let any_art = ctx.rows.iter().any(|r| r.art_uri.is_some());
    let reserve_thumb_column = !thumbs_off && any_art;

    // How many rows fit starting from `top`?
    fn visible_count(rows: &[ThumbRowSpec], top: usize, max_h: u16, big_h: u16, thumbs_off: bool) -> usize {
        let mut h = 0u16;
        let mut count = 0;
        for spec in rows.iter().skip(top) {
            let rh = if spec.is_header {
                1
            } else if !thumbs_off && spec.art_uri.is_some() {
                spec.row_h_override.unwrap_or(big_h)
            } else {
                1
            };
            if h + rh > max_h {
                break;
            }
            h += rh;
            count += 1;
        }
        count.max(1)
    }

    // Reserve a 1-cell column on the right for the rmpc-style scrollbar when
    // the list overflows. When the list fits on screen, the bar is omitted
    // and the rows reclaim the column.
    let total = ctx.rows.len();
    // Compute scrollbar need with current top + viewport.
    let visible0 = visible_count(ctx.rows, *ctx.top, inner.height, big_h, thumbs_off);
    let needs_scrollbar = total > visible0 && inner.width > 2;
    let inner = if needs_scrollbar {
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width.saturating_sub(1),
            height: inner.height,
        }
    } else {
        inner
    };

    // Auto-scroll: keep cursor visible. Walk from `top` to bound visible
    // window; if cursor outside, pull `top` toward cursor.
    if ctx.cursor < *ctx.top {
        *ctx.top = ctx.cursor;
    } else {
        loop {
            let vis = visible_count(ctx.rows, *ctx.top, inner.height, big_h, thumbs_off);
            if ctx.cursor < *ctx.top + vis || *ctx.top >= ctx.rows.len().saturating_sub(1) {
                break;
            }
            *ctx.top += 1;
            if *ctx.top > ctx.cursor {
                *ctx.top = ctx.cursor;
                break;
            }
        }
    }

    let visible = visible_count(ctx.rows, *ctx.top, inner.height, big_h, thumbs_off);
    ctx.visible_row_heights.clear();
    ctx.thumb_hits.clear();

    let mut y_off: u16 = 0;
    for i in 0..visible {
        let idx = *ctx.top + i;
        if idx >= ctx.rows.len() {
            break;
        }
        let spec = &ctx.rows[idx];
        let this_h = row_h_for(spec);
        ctx.visible_row_heights.push(this_h);

        let row_rect = Rect {
            x: inner.x,
            y: inner.y + y_off,
            width: inner.width,
            height: this_h,
        };
        y_off += this_h;

        let row_has_thumb = !thumbs_off && spec.art_uri.is_some();
        let (thumb_rect, text_rect) = if spec.is_header {
            // Headers span the full row width, ignoring the thumb column
            // reserved by neighbouring track rows.
            (None, row_rect)
        } else if reserve_thumb_column {
            let img_w = (ctx.thumb_cells * 2).min(row_rect.width.saturating_sub(2));
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(img_w),
                    Constraint::Length(1),
                    Constraint::Min(1),
                ])
                .split(row_rect);
            // Only render thumb if this row has art; otherwise leave the
            // column blank but still aligned with neighbours.
            (
                if row_has_thumb { Some(split[0]) } else { None },
                split[2],
            )
        } else {
            (None, row_rect)
        };

        let highlight = idx == ctx.cursor;

        if let (Some(thumb_rect), Some(uri)) = (thumb_rect, spec.art_uri.as_deref()) {
            // Record the rect so the app's mouse handler can expand the
            // cover full-screen on click.
            ctx.thumb_hits.push((thumb_rect, uri.to_string()));
            ensure_protocol(uri, art_cache, picker, protocols);
            if !protocols.contains_key(uri) {
                spawn_fetch(
                    uri,
                    spec.source_scheme,
                    art_cache,
                    dispatcher,
                    fetching,
                    wake,
                );
            }
            if let Some(p) = protocols.get_mut(uri) {
                f.render_stateful_widget(StatefulImage::default(), thumb_rect, p);
            } else {
                let placeholder = Paragraph::new("..").style(ctx.theme.dim());
                f.render_widget(placeholder, thumb_rect);
            }
        } else if let Some(thumb_rect) = thumb_rect {
            f.render_widget(Block::default(), thumb_rect);
        }

        // Selection (cursor on row) wins as a full-bar highlight. Otherwise,
        // a now-playing row gets its text styled in accent + bold so the user
        // can locate the playing track at a glance without the bar moving.
        let row_style = if highlight {
            ctx.theme.selection()
        } else if spec.is_header {
            ctx.theme.header()
        } else if spec.is_now_playing {
            ctx.theme
                .accent()
                .add_modifier(ratatui::style::Modifier::BOLD)
        } else {
            ctx.theme.fg()
        };
        // Render pin marker (left edge of text area) when row is pinned.
        // 2-cell prefix: "▸ " in accent + bold. Text rect is shrunk by 2
        // so columns still align after the marker.
        let text_rect = if spec.pinned && text_rect.width > 3 {
            let marker_rect = Rect {
                x: text_rect.x,
                y: text_rect.y,
                width: 2,
                height: text_rect.height,
            };
            let pin_style = ctx
                .theme
                .accent()
                .add_modifier(ratatui::style::Modifier::BOLD);
            f.render_widget(Paragraph::new("▸ ").style(pin_style), marker_rect);
            Rect {
                x: text_rect.x + 2,
                y: text_rect.y,
                width: text_rect.width - 2,
                height: text_rect.height,
            }
        } else {
            text_rect
        };
        match spec.track_cols.as_ref() {
            Some(cols) => render_track_columns(f, text_rect, cols, row_style),
            None => {
                let para = Paragraph::new(spec.label.clone()).style(row_style);
                f.render_widget(para, text_rect);
            }
        }
    }

    if needs_scrollbar {
        // Clamp the bar's bottom so it stops above the album-art panel; the
        // panel renders later and would overdraw the bar otherwise.
        let bar_height = match ctx.art_top_y {
            Some(y) if y > inner.y => y.saturating_sub(inner.y).min(inner.height),
            _ => inner.height,
        };
        if bar_height < 2 {
            return;
        }
        let bar_area = Rect {
            x: inner.x + inner.width,
            y: inner.y,
            width: 1,
            height: bar_height,
        };
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(ctx.theme.dim())
            .thumb_style(ctx.theme.accent());
        let mut state = ScrollbarState::new(total.saturating_sub(visible))
            .position(*ctx.top)
            .viewport_content_length(visible);
        f.render_stateful_widget(bar, bar_area, &mut state);
    }
}

/// Truncate `s` to fit in `width` cells. If overflowing, drop trailing
/// chars and append `…` so the column ends with a visible separator dot
/// instead of slamming into the next column. Returns owned String so
/// callers can hand it to `Paragraph::new`.
fn truncate_col(s: &str, width: u16) -> String {
    let w = width as usize;
    if w == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= w {
        return s.to_string();
    }
    if w == 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(w.saturating_sub(2)).collect();
    out.push('…');
    out
}

/// Lay out a single track row as 4 columns: Artist | Title | Album | Len.
/// Last column is fixed-width and right-aligned. Other three split the
/// remaining width as 30 / 35 / 30 percent. Each variable column reserves
/// 1 cell of trailing padding and any overflow is replaced with `…`, so
/// long fields end with a visible gap to the next column instead of
/// merging into it.
fn render_track_columns(
    f: &mut Frame<'_>,
    area: Rect,
    cols: &TrackColumns,
    style: ratatui::style::Style,
) {
    if area.width < 12 {
        // Too narrow for columns — fall back to `Artist — Title`.
        let label = if cols.artist.is_empty() {
            cols.title.clone()
        } else {
            format!("{} — {}", cols.artist, cols.title)
        };
        let label = truncate_col(&label, area.width);
        f.render_widget(Paragraph::new(label).style(style), area);
        return;
    }
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30),
            Constraint::Percentage(35),
            Constraint::Percentage(30),
            Constraint::Length(6),
            // 1-cell trailing spacer so the duration doesn't run flush
            // against the panel border / scrollbar.
            Constraint::Length(1),
        ])
        .split(area);
    let dur = cols
        .duration
        .map(|d| {
            let secs = d.as_secs();
            format!("{}:{:02}", secs / 60, secs % 60)
        })
        .unwrap_or_else(|| "—".into());
    // Reserve 1 trailing cell so adjacent columns keep visible whitespace.
    let pad = |w: u16| -> u16 { w.saturating_sub(1).max(1) };
    let artist = truncate_col(&cols.artist, pad(split[0].width));
    let title = truncate_col(&cols.title, pad(split[1].width));
    let album = truncate_col(&cols.album, pad(split[2].width));
    f.render_widget(Paragraph::new(artist).style(style), split[0]);
    f.render_widget(Paragraph::new(title).style(style), split[1]);
    f.render_widget(Paragraph::new(album).style(style), split[2]);
    f.render_widget(
        Paragraph::new(dur).style(style).alignment(Alignment::Right),
        split[3],
    );
}

fn ensure_protocol(
    uri: &str,
    art_cache: &Arc<ArtCache>,
    picker: &Picker,
    protocols: &mut HashMap<String, StatefulProtocol>,
) {
    if protocols.contains_key(uri) {
        return;
    }
    if let Some(img) = art_cache.peek(uri) {
        protocols.insert(uri.to_string(), picker.new_resize_protocol((*img).clone()));
    }
}

fn spawn_fetch(
    uri: &str,
    source_scheme: Option<&'static str>,
    art_cache: &Arc<ArtCache>,
    dispatcher: &Dispatcher,
    fetching: &mut HashSet<String>,
    wake: &UnboundedSender<()>,
) {
    if fetching.contains(uri) {
        return;
    }
    let Some(scheme) = source_scheme else {
        return;
    };
    let Some(src) = dispatcher.get(scheme).cloned() else {
        return;
    };
    fetching.insert(uri.to_string());
    let cache = art_cache.clone();
    let key = uri.to_string();
    let wake = wake.clone();
    tokio::spawn(async move {
        let res = cache
            .get(&key, || async { src.art(&key, ArtSize::Thumb).await })
            .await;
        if let Err(e) = res {
            tracing::debug!("thumb fetch failed for {key}: {e}");
        }
        let _ = wake.send(());
    });
}
