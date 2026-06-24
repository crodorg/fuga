use std::time::Duration;

use image::imageops::FilterType;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Tabs, Wrap};
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, LibraryView};
use crate::config::TabAlignment;
use crate::theme::Theme;
use crate::types::{Category, PlayState};
use crate::widgets::thumb_list::{ThumbListCtx, ThumbRowSpec, render_thumb_list};

/// Build a `ThumbRowSpec` from a browse `Entry`. Track entries get the
/// rmpc-style 4-column layout; everything else (album/artist/playlist
/// landing rows, "load more" sentinels) renders the plain label.
fn entry_to_row(
    e: &crate::types::Entry,
    scheme: &'static str,
    now_playing: bool,
    pinned: bool,
) -> ThumbRowSpec {
    use crate::types::EntryKind;
    let art_uri = e.display.as_ref().and_then(|d| d.art_uri.clone());
    let track_cols = match (&e.kind, e.display.as_ref()) {
        (EntryKind::Track, Some(d)) => Some(crate::widgets::thumb_list::TrackColumns {
            artist: d.artist.clone().unwrap_or_default(),
            title: d.title.clone(),
            album: d.album.clone().unwrap_or_default(),
            duration: d.duration,
        }),
        _ => None,
    };
    ThumbRowSpec {
        label: e.label.clone(),
        art_uri,
        source_scheme: Some(scheme),
        track_cols,
        is_now_playing: now_playing,
        pinned,
        is_header: false,
        row_h_override: None,
    }
}

/// `(uri, scheme)` of the currently-playing queue item, if any. Used to
/// flag the matching browse / queue row for now-playing highlight.
fn playing_key(app: &App) -> Option<(String, &'static str)> {
    app.queue
        .current()
        .map(|q| (q.uri.clone(), q.source_scheme))
}

pub fn render(app: &mut App, f: &mut Frame<'_>) {
    let area = f.area();
    // 6 rows: 4 text rows (title/artist/album/source) + progress + volume.
    // Shuffle/repeat icons share the volume row at left.
    let bottom_h: u16 = 6;

    // Standard 3-row stack: tabs / body / bottom bar.
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(bottom_h),
        ])
        .split(area);
    let tabs_area = layout[0];
    let body_area = layout[1];
    let bottom_full = layout[2];

    // Compute art-panel size (or None when not playable / too tight).
    let art_dims = compute_art_dims(app, area, bottom_h);

    // Bottom bar shrinks horizontally so nothing renders beneath the album
    // cover; the art panel will overlay the bottom-right corner of the
    // terminal, protruding upward into the body area.
    let bottom_area = match art_dims {
        Some((art_w, _)) if bottom_full.width > art_w + 4 => Rect {
            x: bottom_full.x,
            y: bottom_full.y,
            width: bottom_full.width.saturating_sub(art_w),
            height: bottom_full.height,
        },
        _ => bottom_full,
    };

    record_tab_rects(app, tabs_area);
    render_tabs(app, f, tabs_area);

    // Where the art panel will land — passed to the body so the scrollbar
    // shrinks to end just above the panel's top border.
    let art_top_y: Option<u16> = art_dims.map(|(_, h)| area.bottom().saturating_sub(h));

    app.body_rect = Some(body_area);
    if app.lyrics_visible {
        render_lyrics(app, f, body_area);
    } else {
        match app.active_category() {
            Category::Queue => render_queue(app, f, body_area, art_top_y),
            Category::Search => render_search(app, f, body_area),
            _ => render_browse(app, f, body_area, art_top_y),
        }
    }
    app.volume_rect = None;
    render_bottom_bar(app, f, bottom_area);
    app.art_panel_rect = None;
    if let Some((art_w, art_h)) = art_dims {
        render_art_panel(app, f, area, art_w, art_h, bottom_h);
    }

    if app.status.is_some() {
        render_status_toast(app, f, area);
    }
    if app.help_visible {
        render_help(app, f, area);
    }
    if app.device_modal_open {
        render_device_modal(app, f, area);
    }
    if app.sort_modal_open {
        render_sort_modal(app, f, area);
    }
    if app.command_input_focused {
        render_command_bar(app, f, area);
    }
    if app.action_menu_open {
        render_action_menu(app, f, area);
    }
    if app.playlist_picker.is_some() {
        render_playlist_picker(app, f, area);
    }
    // Expanded-art overlay sits on top of everything. Rendered last so it
    // covers tab bar, body, bottom bar.
    if app.expanded_art_uri.is_some() {
        render_expanded_art(app, f, area);
    }
}

fn render_action_menu(app: &App, f: &mut Frame<'_>, area: Rect) {
    let labels = app.action_menu_labels();
    if labels.is_empty() {
        return;
    }
    let w = 38u16.min(area.width.saturating_sub(4));
    let h = (labels.len() as u16 + 2).min(area.height.saturating_sub(4));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(
            " actions — j/k Enter Esc ",
            app.theme.accent(),
        ))
        .borders(Borders::ALL)
        .border_style(app.theme.block_border());
    let lines: Vec<Line<'_>> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let cursor = if i == app.action_menu_sel { ">" } else { " " };
            let style = if i == app.action_menu_sel {
                app.theme.selection()
            } else {
                app.theme.fg()
            };
            Line::from(vec![
                Span::styled(format!("{cursor} "), app.theme.accent()),
                Span::styled((*label).to_string(), style),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Add-to-playlist picker modal. Renders a centered list of the user's
/// writable Spotify playlists. j/k to navigate, Enter to commit, Esc to
/// cancel. Track URI is held in `app.playlist_picker`.
fn render_playlist_picker(app: &App, f: &mut Frame<'_>, area: Rect) {
    let Some(p) = app.playlist_picker.as_ref() else {
        return;
    };
    if p.entries.is_empty() {
        let popup = Rect {
            x: area.x + area.width / 4,
            y: area.y + area.height / 3,
            width: area.width / 2,
            height: 5,
        };
        f.render_widget(Clear, popup);
        let block = Block::default()
            .title(Span::styled(" add to playlist ", app.theme.accent()))
            .borders(Borders::ALL)
            .border_style(app.theme.block_border());
        f.render_widget(
            Paragraph::new("no writable playlists found").block(block),
            popup,
        );
        return;
    }
    let max_h = (p.entries.len() as u16 + 2).min(area.height.saturating_sub(4));
    let w = 60u16.min(area.width.saturating_sub(4));
    let h = max_h.max(5);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(
            " add to playlist — j/k Enter Esc ",
            app.theme.accent(),
        ))
        .borders(Borders::ALL)
        .border_style(app.theme.block_border());
    let visible_h = popup.height.saturating_sub(2) as usize;
    // Naive scroll: keep sel in view.
    let top = p.sel.saturating_sub(visible_h.saturating_sub(1));
    let lines: Vec<Line<'_>> = p
        .entries
        .iter()
        .enumerate()
        .skip(top)
        .take(visible_h)
        .map(|(i, e)| {
            let cursor = if i == p.sel { ">" } else { " " };
            let style = if i == p.sel {
                app.theme.selection()
            } else {
                app.theme.fg()
            };
            Line::from(vec![
                Span::styled(format!("{cursor} "), app.theme.accent()),
                Span::styled(e.label.clone(), style),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Expanded art overlay. Rect is sized to the image's natural aspect (no
/// arbitrary letterbox) capped at ~60% of the terminal area, and shifted
/// LEFT when the default centered position would overlap the now-playing
/// art panel in the bottom-right.
fn render_expanded_art(app: &mut App, f: &mut Frame<'_>, area: Rect) {
    let Some(uri) = app.expanded_art_uri.clone() else {
        return;
    };
    // Build a dedicated protocol for the overlay. Sharing the inline
    // thumb's protocol (keyed by uri in `app.protocols`) caused a
    // single-frame top-left zoom artifact in the thumb because the
    // resize state would flip from small-rect to large-rect mid-frame.
    let needs_build = match &app.expanded_art_protocol {
        Some((u, _)) => u != &uri,
        None => true,
    };
    if needs_build {
        if let Some(img) = app.art_cache.peek(&uri) {
            let proto = app.term.picker.new_resize_protocol((*img).clone());
            app.expanded_art_protocol = Some((uri.clone(), proto));
        }
    }

    // Draw nothing until the full-size image is decoded and cached. The fetch
    // is kicked in `expand_hovered_art`; until it lands, drawing would pop the
    // border up at the 60% budget rect and then snap to the image's true size
    // once it loads. The overlay sizes itself to the image, so it can only be
    // drawn once the image exists.
    if app.art_cache.peek(&uri).is_none() {
        return;
    }

    // Budget rect: 60% of terminal. Then ask ratatui-image's own resize
    // pipeline what cells the Fit-scaled image will actually paint. That
    // way the border wraps the painted image flush — no rounding-induced
    // whitespace on the right/bottom.
    let budget_w = ((area.width as f64 * 0.6).max(20.0).min(area.width as f64)) as u16;
    let budget_h = ((area.height as f64 * 0.6).max(8.0).min(area.height as f64)) as u16;

    let (inner_w_cells, inner_h_cells) = if let Some(img) = app.art_cache.peek(&uri) {
        let font_size = app.term.picker.font_size();
        let source = ratatui_image::protocol::ImageSource::new(
            (*img).clone(),
            font_size,
            image::Rgba([0, 0, 0, 0]),
        );
        let avail = Rect::new(0, 0, budget_w.saturating_sub(2), budget_h.saturating_sub(2));
        let rect = Resize::Fit(None).render_area(&source, font_size, avail);
        (rect.width.max(1), rect.height.max(1))
    } else {
        (
            budget_w.saturating_sub(2).max(1),
            budget_h.saturating_sub(2).max(1),
        )
    };
    let w = inner_w_cells.saturating_add(2);
    let h = inner_h_cells.saturating_add(2);

    // Center horizontally + vertically.
    let mut x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;

    // Shift left if the overlay would overlap the now-playing art panel.
    if let Some(np) = app.art_panel_rect {
        let overlay_right = x + w;
        if overlay_right > np.x && y + h > np.y && x < np.x + np.width && y < np.y + np.height {
            // Move overlay leftward so its right edge stops 2 cells before
            // the now-playing panel. Clamp to area.x.
            let target_right = np.x.saturating_sub(2);
            if target_right > area.x + w {
                x = target_right - w;
            } else {
                x = area.x;
            }
        }
    }

    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .title(Span::styled(
            " art — any key/click to close ",
            app.theme.accent(),
        ))
        .borders(Borders::ALL)
        .border_style(app.theme.block_border());
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if let Some((_, proto)) = app.expanded_art_protocol.as_mut() {
        let img = StatefulImage::default().resize(Resize::Fit(None));
        f.render_stateful_widget(img, inner, proto);
    } else {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "loading full-size art…",
                app.theme.dim(),
            ))),
            inner,
        );
    }
}

/// Carve the tab-bar rect into per-tab cells so the mouse layer can hit-test.
/// Mirrors how `Tabs` widget lays out the title row inside the borders.
/// Honors `tab_alignment` so click targets land on the visible labels even
/// when the bar is centered or right-aligned.
fn record_tab_rects(app: &mut App, area: Rect) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    app.tab_rects.clear();
    if app.tabs.is_empty() {
        return;
    }
    // ratatui's Tabs widget uses " label " (label + 2 surrounding spaces) per
    // tab and "│" (1 cell) as separator. Total width matches that layout.
    let mode = app.active_source;
    let widths: Vec<u16> = app
        .tabs
        .iter()
        .map(|c| c.label_for(mode).chars().count() as u16 + 2)
        .collect();
    let total: u16 = widths.iter().copied().sum::<u16>() + (widths.len().saturating_sub(1)) as u16; // separators
    let mut x = match app.tab_alignment {
        TabAlignment::Left => inner.x.saturating_add(1),
        TabAlignment::Center => inner
            .x
            .saturating_add(inner.width.saturating_sub(total) / 2),
        TabAlignment::Right => inner
            .x
            .saturating_add(inner.width.saturating_sub(total))
            .saturating_sub(1),
    };
    for (i, w) in widths.iter().enumerate() {
        let rect = Rect {
            x,
            y: inner.y,
            width: *w,
            height: inner.height,
        };
        app.tab_rects.push((rect, i));
        x = x.saturating_add(*w).saturating_add(1);
    }
}

fn render_status_toast(app: &App, f: &mut Frame<'_>, area: Rect) {
    if area.height < 4 || area.width < 8 {
        return;
    }
    let Some(s) = app.status.as_deref().filter(|s| !s.is_empty()) else {
        return;
    };
    let label = format!(" {s} ");
    // Toast sits on the body block's TOP border row (y = tabs_h = 3), pinned
    // to the right edge so it never overlaps the panel title's `(N)` count
    // on the left. Reserve a 2-cell margin from the right border.
    let label_w = (label.chars().count() as u16).min(area.width.saturating_sub(4));
    let row = Rect {
        x: area.x + area.width.saturating_sub(label_w + 2),
        y: area.y + 3,
        width: label_w,
        height: 1,
    };
    f.render_widget(Clear, row);
    let line = Line::from(vec![Span::styled(
        label,
        app.theme.accent().add_modifier(Modifier::REVERSED),
    )]);
    f.render_widget(Paragraph::new(line).style(app.theme.fg()), row);
}

/// Vim-style command bar — single row at the very bottom of the screen,
/// drawn on top of the bottom bar's first row when active.
fn render_command_bar(app: &App, f: &mut Frame<'_>, area: Rect) {
    if area.height < 2 {
        return;
    }
    let row = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    f.render_widget(Clear, row);
    let line = Line::from(vec![
        Span::styled(":", app.theme.accent()),
        Span::styled(app.command_buffer.clone(), app.theme.fg()),
        Span::styled("_", app.theme.accent().add_modifier(Modifier::SLOW_BLINK)),
    ]);
    f.render_widget(Paragraph::new(line), row);
}

fn render_tabs(app: &App, f: &mut Frame<'_>, area: Rect) {
    let mode = app.active_source;
    let titles: Vec<Line<'_>> = app
        .tabs
        .iter()
        .map(|c| Line::from(Span::styled(c.label_for(mode), app.theme.fg())))
        .collect();
    let alignment = match app.tab_alignment {
        TabAlignment::Left => Alignment::Left,
        TabAlignment::Center => Alignment::Center,
        TabAlignment::Right => Alignment::Right,
    };
    // Mode badge in the top-left title slot. Colored to match the active
    // source's palette so the user always sees which mode the borders belong
    // to without scanning the screen.
    let badge = format!(" {} ", mode.label());
    let block = Block::default()
        .title(Line::from(Span::styled(badge, app.theme.accent())))
        .title_alignment(alignment)
        .borders(Borders::ALL)
        .border_style(app.theme.block_border());
    let _ = alignment;
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.tabs.is_empty() {
        return;
    }
    let widths: Vec<u16> = app
        .tabs
        .iter()
        .map(|c| c.label_for(mode).chars().count() as u16 + 2)
        .collect();
    let total: u16 = widths.iter().copied().sum::<u16>() + (widths.len().saturating_sub(1)) as u16;
    let pad = inner.width.saturating_sub(total);
    let (left_pad, right_pad) = match app.tab_alignment {
        TabAlignment::Left => (0u16, pad),
        TabAlignment::Center => (pad / 2, pad - pad / 2),
        TabAlignment::Right => (pad, 0),
    };
    let split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_pad),
            Constraint::Length(total),
            Constraint::Length(right_pad),
        ])
        .split(inner);
    let center = split[1];
    let tabs = Tabs::new(titles)
        .select(app.active_tab_idx)
        .highlight_style(app.theme.selection());
    f.render_widget(tabs, center);
}

fn render_browse(app: &mut App, f: &mut Frame<'_>, area: Rect, art_top_y: Option<u16>) {
    let base_title = app.current_view_title();
    let cat = app.active_category();
    // Borrow the current view rather than deep-cloning it. `rows` below is
    // owned, so this borrow ends before the later `&mut app` mutations
    // (set_filtered_browse_indices, body_top_at_render, …) — which is the only
    // reason the clone existed. Drops one full per-frame copy of every Entry's
    // strings; rows are built straight from the source.
    let view = app.category_states.get(&cat).and_then(|s| s.stack.last());

    let pk = playing_key(app);
    let now_playing_match = |scheme: &'static str, uri: &str| -> bool {
        match &pk {
            Some((u, s)) => *s == scheme && u == uri,
            None => false,
        }
    };
    let rows: Vec<ThumbRowSpec> = match view {
        Some(LibraryView::Entries {
            scheme, entries, ..
        }) => entries
            .iter()
            .map(|e| {
                entry_to_row(
                    e,
                    scheme,
                    now_playing_match(scheme, &e.uri),
                    app.pinned.contains(&e.uri),
                )
            })
            .collect(),
        Some(LibraryView::Tracks { items, .. }) => items
            .iter()
            .map(|it| ThumbRowSpec {
                label: it.display.title.clone(),
                art_uri: it.display.art_uri.clone().or_else(|| Some(it.uri.clone())),
                source_scheme: Some("local"),
                track_cols: Some(crate::widgets::thumb_list::TrackColumns {
                    artist: it.display.artist.clone().unwrap_or_default(),
                    title: it.display.title.clone(),
                    album: it.display.album.clone().unwrap_or_default(),
                    duration: it.display.duration,
                }),
                is_now_playing: now_playing_match("local", &it.uri),
                pinned: app.pinned.contains(&it.uri),
                is_header: false,
                row_h_override: None,
            })
            .collect(),
        Some(LibraryView::Sections { sections, .. }) => {
            let mut out: Vec<ThumbRowSpec> = Vec::new();
            for sec in sections {
                out.push(ThumbRowSpec::plain(
                    format!("── {} ──", sec.display_name),
                    None,
                    Some(sec.scheme),
                ));
                for e in &sec.entries {
                    out.push(entry_to_row(
                        e,
                        sec.scheme,
                        now_playing_match(sec.scheme, &e.uri),
                        app.pinned.contains(&e.uri),
                    ));
                }
            }
            out
        }
        None => vec![ThumbRowSpec::plain(
            format!("loading {}…", cat.label()),
            None,
            None,
        )],
    };

    // body_row_heights populated by the renderer below; per-row heights now
    // vary with whether each row has art.

    // Apply the in-view filter (`/`) here so the browse path mirrors the
    // queue path: rows shown to the user are the filtered subset, and we
    // cache the original-row indices on App for activate/enqueue to remap
    // the cursor after the user picks a row.
    let filter_buf = app.filter_input.clone();
    let active_filter = app.current_filter().map(str::to_owned);
    let q_lower = filter_buf
        .clone()
        .or(active_filter.clone())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    let (rows, indices_owned): (Vec<ThumbRowSpec>, Option<Vec<usize>>) = match q_lower {
        Some(ref q) => {
            let mut filtered = Vec::new();
            let mut indices = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                if crate::widgets::thumb_list::row_matches(r, q) {
                    filtered.push(r.clone());
                    indices.push(i);
                }
            }
            (filtered, Some(indices))
        }
        None => (rows, None),
    };
    app.set_filtered_browse_indices(indices_owned);

    let title = match (&filter_buf, &active_filter) {
        (Some(buf), _) => format!("{base_title}  /{buf}_"),
        (None, Some(committed)) if !committed.is_empty() => {
            format!("{base_title}  /{committed}")
        }
        _ => base_title,
    };
    // Animated `loading…` indicator while the streaming task is feeding
    // rows into this view, rendered as a right-aligned secondary title on
    // the block. tick_counter advances every 250ms so phase cycles ~750ms.
    // "loading" prefix keeps the indicator readable on a busy header; the
    // bare dot dance was easy to miss at the very edge.
    let right_title = if app
        .category_states
        .get(&cat)
        .map(|s| s.streaming)
        .unwrap_or(false)
    {
        let phase = (app.tick_counter as usize) % 3;
        let dots = match phase {
            0 => ".  ",
            1 => ".. ",
            _ => "...",
        };
        Some(format!("loading{dots}"))
    } else {
        None
    };

    let (cursor_raw, mut top) = match app.category_states.get(&cat) {
        Some(s) => (s.cursor, s.top),
        None => (0, 0),
    };
    let cursor = cursor_raw.min(rows.len().saturating_sub(1));
    app.body_top_at_render = top;
    let mut visible_heights: Vec<u16> = Vec::new();
    let mut thumb_hits: Vec<(Rect, String)> = Vec::new();
    let ctx = ThumbListCtx {
        area,
        title,
        rows: &rows,
        cursor,
        top: &mut top,
        thumb_cells: app.thumb_cells,
        mode: app.term.mode,
        theme: &app.theme,
        art_top_y,
        visible_row_heights: &mut visible_heights,
        thumb_hits: &mut thumb_hits,
        count_override: None,
        right_title,
    };
    render_thumb_list(
        f,
        ctx,
        &app.art_cache,
        &app.dispatcher,
        &app.term.picker,
        &mut app.protocols,
        &mut app.fetching,
        &app.wake_tx,
    );
    app.body_row_heights = visible_heights;
    app.thumb_hits = thumb_hits;
    if let Some(s) = app.category_states.get_mut(&cat) {
        s.top = top;
    }
    if let Some(b) = app.body_rect.as_mut() {
        b.x = b.x.saturating_add(1);
        b.y = b.y.saturating_add(1);
        b.width = b.width.saturating_sub(2);
        b.height = b.height.saturating_sub(2);
    }
}

fn render_queue(app: &mut App, f: &mut Frame<'_>, area: Rect, art_top_y: Option<u16>) {
    let cur = app.queue.current_index();
    let filter_indices = app.filtered_queue_indices();
    let all_rows: Vec<ThumbRowSpec> = app
        .queue
        .items()
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let prefix = if Some(i) == cur { "> " } else { "  " };
            let title = format!("{prefix}{}", q.display.title);
            ThumbRowSpec {
                label: format!("{prefix}{}", q.display.title),
                art_uri: q.display.art_uri.clone().or_else(|| Some(q.uri.clone())),
                source_scheme: Some(q.source_scheme),
                track_cols: Some(crate::widgets::thumb_list::TrackColumns {
                    artist: q.display.artist.clone().unwrap_or_default(),
                    title,
                    album: q.display.album.clone().unwrap_or_default(),
                    duration: q.display.duration,
                }),
                is_now_playing: Some(i) == cur,
                pinned: app.pinned.contains(&q.uri),
                is_header: false,
                row_h_override: None,
            }
        })
        .collect();
    let rows: Vec<ThumbRowSpec> = match &filter_indices {
        Some(idx) => idx
            .iter()
            .filter_map(|i| all_rows.get(*i).cloned())
            .collect(),
        None => all_rows,
    };
    let filter_buf = app.filter_input.clone();
    let committed_filter = app
        .current_filter()
        .filter(|_| filter_buf.is_none())
        .map(|s| s.to_string());
    let title = match (&filter_buf, &committed_filter) {
        (Some(buf), _) => format!("Queue  /{buf}_"),
        (None, Some(committed)) => format!("Queue  /{committed}"),
        _ => "Queue".to_string(),
    };
    app.body_top_at_render = app.queue_top;
    let mut visible_heights: Vec<u16> = Vec::new();
    let mut thumb_hits: Vec<(Rect, String)> = Vec::new();
    let ctx = ThumbListCtx {
        area,
        title,
        rows: &rows,
        cursor: app.queue_cursor,
        top: &mut app.queue_top,
        thumb_cells: app.thumb_cells,
        mode: app.term.mode,
        theme: &app.theme,
        art_top_y,
        visible_row_heights: &mut visible_heights,
        thumb_hits: &mut thumb_hits,
        count_override: None,
        right_title: None,
    };
    render_thumb_list(
        f,
        ctx,
        &app.art_cache,
        &app.dispatcher,
        &app.term.picker,
        &mut app.protocols,
        &mut app.fetching,
        &app.wake_tx,
    );
    app.body_row_heights = visible_heights;
    app.thumb_hits = thumb_hits;
    if let Some(b) = app.body_rect.as_mut() {
        b.x = b.x.saturating_add(1);
        b.y = b.y.saturating_add(1);
        b.width = b.width.saturating_sub(2);
        b.height = b.height.saturating_sub(2);
    }
}

fn render_search(app: &mut App, f: &mut Frame<'_>, area: Rect) {
    // Top: input box. Bottom: flattened result list (group headers + items)
    // rendered through the same thumb_list widget as browse / queue so the
    // search view shows inline thumbnails for every result row.
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);

    let input_block = Block::default()
        .title(Span::styled(
            if app.search_input_focused {
                " search (Enter=run, Esc=cancel) "
            } else {
                " search (s to focus) "
            },
            app.theme.accent(),
        ))
        .borders(Borders::ALL)
        .border_style(app.theme.block_border());
    let input_cursor = if app.search_input_focused { "_" } else { "" };
    let input = Paragraph::new(format!("{}{input_cursor}", app.search_query))
        .style(app.theme.fg())
        .block(input_block);
    f.render_widget(input, split[0]);

    let body_area = split[1];

    if app.search_results.is_empty() {
        let body_block = Block::default()
            .title(Span::styled(" results ", app.theme.accent()))
            .borders(Borders::ALL)
            .border_style(app.theme.block_border());
        let inner = body_block.inner(body_area);
        f.render_widget(body_block, body_area);
        let hint = if app.search_query.is_empty() {
            "press s to start a search"
        } else {
            "no results"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, app.theme.dim()))),
            inner,
        );
        return;
    }

    // Build a flat row list. Search is mode-driven (one group per active
    // source), and the tab bar / source theme already communicate context,
    // so no group header lines are rendered — every row is a result.
    let mut rows: Vec<ThumbRowSpec> = Vec::new();
    for group in &app.search_results {
        for item in &group.items {
            let label = match (&item.display.artist, &item.display.album) {
                (Some(a), Some(al)) => format!("{a} — {al} — {}", item.display.title),
                (Some(a), None) => format!("{a} — {}", item.display.title),
                _ => item.display.title.clone(),
            };
            rows.push(ThumbRowSpec {
                label,
                art_uri: item
                    .display
                    .art_uri
                    .clone()
                    .or_else(|| Some(item.uri.clone())),
                source_scheme: Some(group.scheme),
                track_cols: None,
                is_now_playing: false,
                pinned: false,
                is_header: false,
                row_h_override: None,
            });
        }
    }
    let data_count = rows.len();

    let mut visible_heights: Vec<u16> = Vec::new();
    let mut thumb_hits: Vec<(Rect, String)> = Vec::new();
    let ctx = ThumbListCtx {
        area: body_area,
        title: "results".into(),
        rows: &rows,
        cursor: app.search_cursor,
        top: &mut app.search_top,
        thumb_cells: app.thumb_cells,
        mode: app.term.mode,
        theme: &app.theme,
        art_top_y: None,
        visible_row_heights: &mut visible_heights,
        thumb_hits: &mut thumb_hits,
        count_override: Some(data_count),
        right_title: None,
    };
    render_thumb_list(
        f,
        ctx,
        &app.art_cache,
        &app.dispatcher,
        &app.term.picker,
        &mut app.protocols,
        &mut app.fetching,
        &app.wake_tx,
    );
    app.body_row_heights = visible_heights;
    app.thumb_hits = thumb_hits;
}

fn render_bottom_bar(app: &mut App, f: &mut Frame<'_>, area: Rect) {
    // Tint the WHOLE now-playing block — border, accent, volume, shuffle,
    // repeat, progress bar — by the *playing* track's source, not the active
    // browse mode. So while you browse YouTube with Spotify playing, the
    // entire bottom row stays green. Cloned to an owned Theme so it doesn't
    // borrow `app` across the rect mutations below.
    let pt = app.playing_theme().into_owned();
    let playing_border = pt.block_border();
    let playing_accent = pt.accent();

    // No title — state moved next to volume. Title slot otherwise left a
    // single-cell gap in the top border where the play/pause glyph used
    // to live.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(playing_border);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height < 4 {
        // Tiny terminal — fall back to single line of progress so the bar
        // still tells the user "what + how far". Skip the right-column meta.
        app.progress_bar_rect = render_progress_row(app, f, inner, &pt);
        app.volume_rect = None;
        return;
    }

    // Layout: 4 stacked rows. The first three are info (left text + right
    // meta), the last is the full-width progress bar. Each info row pairs a
    // text field on the left with a small status field on the right so the
    // progress bar can claim the full width below.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title  | volume
            Constraint::Length(1), // artist | shuf · rep
            Constraint::Length(1), // album  | SPT · OGG · 320
            Constraint::Min(1),    // progress
        ])
        .split(inner);

    let (title, artist, album) = match app.queue.current() {
        Some(q) => {
            // Prefer live stream title (ICY StreamTitle for SomaFM / radio).
            // When it differs from the queue title (station name), the queue
            // title slides into the album slot so both are visible.
            let live = app
                .playback
                .as_ref()
                .and_then(|p| p.stream_title.as_ref())
                .filter(|t| !t.is_empty())
                .filter(|t| t.as_str() != q.display.title);
            let (title, album_override) = match live {
                Some(live) => (live.clone(), Some(q.display.title.clone())),
                None => (q.display.title.clone(), None),
            };
            (
                title,
                q.display.artist.clone().unwrap_or_default(),
                album_override
                    .or_else(|| q.display.album.clone())
                    .unwrap_or_default(),
            )
        }
        None => ("(idle)".into(), String::new(), String::new()),
    };

    // Row 0: title (accent) | `<<  [playing]  >>  VOL`.
    // `<<` and `>>` glyphs are click targets for prev/next; the state
    // label is the click target for play/pause. Liked badge moved to
    // row 1 so this row is purely transport + volume.
    let (state_label, state_style) = match app.playback.as_ref().map(|p| p.state) {
        Some(PlayState::Playing) => ("[playing]", playing_accent),
        Some(PlayState::Paused) => ("[paused] ", app.theme.dim()),
        _ => ("[stopped]", app.theme.dim()),
    };
    let vol_str = format!("VOL {:>3}%", app.master_volume);
    let dim = app.theme.dim();
    let row0_right = Line::from(vec![
        Span::styled("<<", playing_accent),
        Span::raw("  "),
        Span::styled(state_label, state_style),
        Span::raw("  "),
        Span::styled(">>", playing_accent),
        Span::raw("  "),
        Span::styled(vol_str, pt.volume()),
    ]);
    render_text_with_right(f, rows[0], &title, playing_accent, row0_right);

    // Transport widget rects. The right cell sample is the full row-0
    // right string; widget offsets within it map to the glyph spans.
    const ROW0_RIGHT_SAMPLE: &str = "<<  [playing]  >>  VOL 100%";
    let row0_right_rect = right_cell_rect(rows[0], ROW0_RIGHT_SAMPLE);
    if row0_right_rect.width >= ROW0_RIGHT_SAMPLE.len() as u16 {
        let base_x = row0_right_rect.x;
        let y = row0_right_rect.y;
        app.prev_rect = Some(Rect {
            x: base_x,
            y,
            width: 2,
            height: 1,
        });
        app.playpause_rect = Some(Rect {
            x: base_x + 4,
            y,
            width: 9,
            height: 1,
        });
        app.next_rect = Some(Rect {
            x: base_x + 15,
            y,
            width: 2,
            height: 1,
        });
    } else {
        app.prev_rect = None;
        app.playpause_rect = None;
        app.next_rect = None;
    }
    // Volume rect spans the full right cell so scroll-wheel anywhere
    // on the strip nudges volume (existing behavior). Left-clicks on
    // the transport rects above are matched before this rect in the
    // mouse handler.
    app.volume_rect = Some(row0_right_rect);

    // Row 1: artist (fg) | [liked] · shuf · rep. The liked badge sits
    // left of shuf/rep on the right edge.
    let liked_badge = match app.current_liked {
        Some(true) => Some(Span::styled("[*]", playing_accent)),
        Some(false) => Some(Span::styled("[ ]", dim)),
        None => None,
    };
    let mut row1_spans: Vec<Span<'static>> = Vec::new();
    if let Some(badge) = liked_badge {
        row1_spans.push(badge);
        row1_spans.push(Span::raw("  "));
    }
    row1_spans.extend(shuf_rep_spans(app, &pt));
    render_text_with_right(f, rows[1], &artist, app.theme.fg(), Line::from(row1_spans));

    // Row 2: album (dim) | SPT · OGG · 320 kbps. Source abbreviated so the
    // line stays short on narrow terminals; codec + kbps come from
    // PlaybackStatus when the source plumbs them through.
    let source_meta = source_meta_line(app);
    render_text_with_right(f, rows[2], &album, app.theme.dim(), source_meta);

    // Row 3: progress bar — full width.
    app.progress_bar_rect = render_progress_row(app, f, rows[3], &pt);

    // Bounding rect for the three text rows (title / artist / album).
    // Mouse handler maps left/middle/right clicks here to prev / play-pause
    // / next so the now-playing text doubles as a transport surface. The
    // right-cell volume strip on row 0 is matched first in the click
    // handler, so volume clicks still win there.
    app.now_playing_text_rect = Some(Rect {
        x: rows[0].x,
        y: rows[0].y,
        width: rows[0].width,
        height: rows[0].height + rows[1].height + rows[2].height,
    });
}

/// Render the progress bar (and elapsed/total labels) in a single row,
/// returning the inner clickable rect of the bar (None if the row is too
/// narrow). Reused by both the 4-row and degraded layouts.
fn render_progress_row(app: &App, f: &mut Frame<'_>, area: Rect, theme: &Theme) -> Option<Rect> {
    let (elapsed, duration) = app
        .playback
        .as_ref()
        .map(|p| (p.elapsed, p.duration))
        .unwrap_or((Duration::ZERO, None));
    let bar = build_progress_bar(elapsed, duration, area.width as usize, theme);
    f.render_widget(Paragraph::new(bar), area);
    duration.and_then(|d| {
        let elapsed_w = fmt_mmss(elapsed).chars().count() as u16;
        let total_w = fmt_mmss(d).chars().count() as u16;
        let r = progress_bar_inner_rect(area, elapsed_w, total_w);
        (r.width > 0).then_some(r)
    })
}

/// Build the shuffle + repeat span pair. Active states use the accent
/// theme; inactive use dim. Identical to the prior bottom-meter rendering
/// so the styling stays consistent after the layout shift.
fn shuf_rep_spans(app: &App, theme: &Theme) -> Vec<Span<'static>> {
    let shuf_style = if app.shuffle {
        theme.accent()
    } else {
        theme.dim()
    };
    let rep_label = match app.repeat {
        crate::queue::RepeatMode::Off | crate::queue::RepeatMode::All => "REP",
        crate::queue::RepeatMode::Track => "REP\u{00b7}1",
    };
    let rep_style = if matches!(app.repeat, crate::queue::RepeatMode::Off) {
        theme.dim()
    } else {
        theme.accent()
    };
    vec![
        Span::styled("SHUF", shuf_style),
        Span::raw("  "),
        Span::styled(rep_label, rep_style),
    ]
}

/// Build the source-metadata line: `SPT · OGG · 320 kbps`. Codec + bitrate
/// segments are dropped when the active source doesn't supply them.
fn source_meta_line(app: &App) -> Line<'static> {
    let scheme = app.queue.current().map(|q| q.source_scheme).unwrap_or("");
    let mut parts: Vec<String> = vec![short_scheme(scheme).to_string()];
    if let Some(p) = app.playback.as_ref() {
        if let Some(c) = p.codec.as_deref() {
            if !c.is_empty() {
                parts.push(c.to_string());
            }
        }
        if let Some(b) = p.bitrate_kbps {
            if b > 0 {
                parts.push(format!("{b} kbps"));
            }
        }
    }
    Line::from(Span::styled(parts.join(" \u{00b7} "), app.theme.header()))
}

/// Three-letter abbreviations for the bottom-bar source indicator. Tab
/// labels and breadcrumbs continue to use full names from `display_name()`;
/// this is purely a footer-bar shortener so the right column fits when the
/// terminal is narrow.
fn short_scheme(s: &str) -> &'static str {
    match s {
        "spotify" => "SPT",
        "local" => "LOC",
        "somafm" => "SFM",
        "radio" => "RAD",
        "youtube" => "YT",
        "" => "",
        _ => "???",
    }
}

/// Render `left` text on the left and `right` aligned to the right edge of
/// `area`. Truncates the left text with `…` if it would overlap the right
/// column. The right side is a pre-styled `Line` so callers can mix glyph
/// styles (e.g. accent SHUF + dim REP).
fn render_text_with_right(
    f: &mut Frame<'_>,
    area: Rect,
    left: &str,
    left_style: ratatui::style::Style,
    right: Line<'static>,
) {
    if area.width == 0 {
        return;
    }
    let right_w = right
        .spans
        .iter()
        .map(|s| s.content.chars().count())
        .sum::<usize>() as u16;
    // Reserve at least 1 cell of padding between the columns when the left
    // string would otherwise butt up against the right segment.
    let pad: u16 = if right_w > 0 { 1 } else { 0 };
    let left_w = area.width.saturating_sub(right_w + pad);
    let left_rect = Rect {
        x: area.x,
        y: area.y,
        width: left_w,
        height: 1,
    };
    let right_rect = Rect {
        x: area.x + left_w + pad,
        y: area.y,
        width: right_w,
        height: 1,
    };
    let truncated = truncate_with_ellipsis(left, left_w as usize);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(truncated, left_style))),
        left_rect,
    );
    if right_w > 0 {
        f.render_widget(
            Paragraph::new(right).alignment(Alignment::Right),
            right_rect,
        );
    }
}

fn right_cell_rect(area: Rect, sample: &str) -> Rect {
    let w = sample.chars().count() as u16;
    let w = w.min(area.width);
    Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y,
        width: w,
        height: 1,
    }
}

fn truncate_with_ellipsis(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    if width == 1 {
        return "\u{2026}".into();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Compute the inner clickable rect of the progress bar (excluding the
/// elapsed/total time labels). Returned width can be 0 when there's no room.
fn progress_bar_inner_rect(area: Rect, elapsed_w: u16, total_w: u16) -> Rect {
    let label_w = elapsed_w.saturating_add(total_w).saturating_add(2);
    let bar_w = area.width.saturating_sub(label_w);
    Rect {
        x: area.x.saturating_add(elapsed_w).saturating_add(1),
        y: area.y,
        width: bar_w,
        height: 1,
    }
}

/// Compute (art_w, art_h) for the bottom-right art panel, or `None` when art
/// is unavailable or the terminal is too small. Width is derived from height
/// using the live cell pixel ratio so the inner pixel area is square — a
/// square album cover lines up exactly without margin or crop under
/// `Resize::Fit`.
fn compute_art_dims(app: &App, area: Rect, bottom_h: u16) -> Option<(u16, u16)> {
    use crate::term_probe::ThumbMode;
    app.now_playing_protocol.as_ref()?;
    // T-toggle to Off: art panel disappears, body reclaims the space.
    if matches!(app.term.mode, ThumbMode::Off) {
        return None;
    }
    if area.height < bottom_h + 10 || area.width < 50 {
        return None;
    }

    // Collapsed mode (user left-clicked the art): keep art visible but
    // shrunk to bottom-bar height so it no longer protrudes into the list
    // body. Width derived from bottom_h × image aspect so the cover stays
    // proportional.
    if app.art_collapsed {
        let (cw, ch) = app.term.picker.font_size();
        let cell_w_px = cw as f64;
        let cell_h_px = ch as f64;
        let (img_w_px, img_h_px) = app
            .now_playing_aspect
            .map(|(w, h)| (w as f64, h as f64))
            .unwrap_or((1.0, 1.0));
        let img_aspect = if img_h_px > 0.0 {
            img_w_px / img_h_px
        } else {
            1.0
        };
        let inner_h_cells = bottom_h.saturating_sub(1).max(1);
        let inner_h_px = inner_h_cells as f64 * cell_h_px;
        let inner_w_px = inner_h_px * img_aspect;
        let inner_w_cells = (inner_w_px / cell_w_px).round().max(1.0) as u16;
        let art_w = inner_w_cells
            .saturating_add(1)
            .min(area.width.saturating_sub(24))
            .max(6);
        return Some((art_w, bottom_h));
    }

    // Cell pixel dimensions — terminal cells are taller than wide, so we
    // need both axes to keep the image's pixel aspect undistorted when
    // mapped onto the cell grid.
    let (cw, ch) = app.term.picker.font_size();
    let cell_w_px = cw as f64;
    let cell_h_px = ch as f64;

    // Source image aspect (in pixels). Defaults to 1:1 (square) when the
    // image hasn't been decoded yet — matches the previous behavior so
    // album covers (which dominate) keep their square panel.
    let (img_w_px, img_h_px) = app
        .now_playing_aspect
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((1.0, 1.0));
    let img_aspect = if img_h_px > 0.0 {
        img_w_px / img_h_px
    } else {
        1.0
    };

    // Two-axis size knobs from `[ui]`. Each is a percentage of the
    // available space along that axis. Vertical 100% = panel top edge
    // flush against the tab bar's bottom border. Horizontal 100% =
    // panel runs from the 24-cell left-text margin to the right edge.
    // Defaults (70 / 40) preserve the prior look on a typical terminal.
    let h_pct = app.art_height_pct.clamp(20, 100) as u32;
    let w_pct = app.art_width_pct.clamp(15, 100) as u32;

    // The layout in `render` puts a 3-row tab bar above the body, so the
    // panel can grow upward at most `area.height - 3` rows total
    // (including its own top border and the bottom-bar protrusion).
    let available_h = area.height.saturating_sub(3) as u32;
    let available_w = area.width.saturating_sub(24) as u32;

    let max_h = ((h_pct * available_h) / 100) as u16;
    let budget_w: u16 = ((w_pct * available_w) / 100).max(22) as u16;
    let min_h: u16 = 10;

    // Round height to whole cells from the budget width, then back-derive
    // width from THAT height so the inner rect's pixel aspect exactly
    // matches the image. Without the back-derive, the inner rect is wider
    // (or shorter) than aspect demands, so `Resize::Fit` letterboxes and
    // leaves a gap on one edge of the panel.
    let initial_inner_w_px = (budget_w.saturating_sub(1)) as f64 * cell_w_px;
    let initial_inner_h_px = initial_inner_w_px / img_aspect;
    let mut inner_h_cells = (initial_inner_h_px / cell_h_px).round().max(1.0) as u16;

    // Clamp inner height to the panel-height window first.
    let max_inner_h = max_h.saturating_sub(1);
    let min_inner_h = min_h.saturating_sub(1);
    if inner_h_cells > max_inner_h {
        inner_h_cells = max_inner_h;
    } else if inner_h_cells < min_inner_h {
        inner_h_cells = min_inner_h;
    }

    // Back-derive width from rounded height so the inner rect's pixel
    // aspect matches the image; `Resize::Scale` then fills it without
    // distortion.
    let actual_inner_h_px = inner_h_cells as f64 * cell_h_px;
    let actual_inner_w_px = actual_inner_h_px * img_aspect;
    let mut inner_w_cells = (actual_inner_w_px / cell_w_px).round().max(1.0) as u16;

    // If the two knobs disagree (e.g. tall + narrow), the back-derived
    // width may exceed the width budget. Pin width to the budget and
    // recompute height to preserve aspect — round-tripping with .round()
    // here would let width creep back over budget.
    if inner_w_cells.saturating_add(1) > budget_w {
        inner_w_cells = budget_w.saturating_sub(1).max(1);
        let new_inner_w_px = inner_w_cells as f64 * cell_w_px;
        let new_inner_h_px = new_inner_w_px / img_aspect;
        inner_h_cells = (new_inner_h_px / cell_h_px).round().max(1.0) as u16;
    }

    let art_w = inner_w_cells
        .saturating_add(1)
        .min(budget_w)
        .min(area.width.saturating_sub(24))
        .max(16);
    let art_h = inner_h_cells.saturating_add(1);

    if art_w < 16 || art_h < 8 {
        return None;
    }
    Some((art_w, art_h))
}

/// Overlay the art panel anchored to the bottom-right of the terminal. The
/// panel protrudes upward from the bottom bar into the body area: top-left
/// corner is `┌`, top edge `─`, left edge `│`. Where the panel's left edge
/// crosses the bottom-bar's top and bottom border rows, stitch in `┤`
/// connectors so the lines meet cleanly.
fn render_art_panel(
    app: &mut App,
    f: &mut Frame<'_>,
    term: Rect,
    art_w: u16,
    art_h: u16,
    bottom_h: u16,
) {
    let art_rect = Rect {
        x: term.right().saturating_sub(art_w),
        y: term.bottom().saturating_sub(art_h),
        width: art_w,
        height: art_h,
    };
    app.art_panel_rect = Some(art_rect);

    // Tint by the playing source, not the active browse mode — matches the
    // bottom-bar tint so the entire now-playing block reads as one unit.
    let playing_border = app.playing_theme().block_border();

    f.render_widget(Clear, art_rect);

    // Collapsed mode: art sits flush inside the bottom-bar height, no
    // borders. The stray "extra line to the left and above" the user saw
    // was the LEFT + TOP border drawing over the bottom-bar's own border.
    // Render the image into the full art_rect, then return — skip all
    // stitching too.
    if app.art_collapsed {
        if let Some(proto) = app.now_playing_protocol.as_mut() {
            let img = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Lanczos3)));
            f.render_stateful_widget(img, art_rect, proto);
        }
        return;
    }

    // LEFT + TOP borders. Image inner pre-shrunk so it doesn't overlap the
    // bottom-bar's right vertical: art_rect's left column carries the LEFT
    // border, which sits adjacent to (not on top of) the bottom-bar's
    // rightmost interior column. Body area's bottom border meets the art's
    // TOP border via stitching corners below.
    let block = Block::default()
        .borders(Borders::LEFT | Borders::TOP)
        .border_style(playing_border);
    let inner = block.inner(art_rect);
    f.render_widget(block, art_rect);

    if let Some(proto) = app.now_playing_protocol.as_mut() {
        // Scale (not Fit): Fit caps at the image's native size, so a 640px
        // Spotify cover never grew beyond 640px even when the inner rect
        // was ~966px on Retina — leaving large whitespace right and below.
        // Scale fills the inner rect; compute_art_dims sets the inner
        // pixel aspect to match the image, so the fill is undistorted.
        // Lanczos3 filter: Nearest is fast but visibly chunky when scaling
        // a 640px cover up ~50%; Lanczos3 is the standard sharp-but-clean
        // choice and the per-frame cost is negligible for one image.
        let img = StatefulImage::default().resize(Resize::Scale(Some(FilterType::Lanczos3)));
        f.render_stateful_widget(img, inner, proto);
    }

    // Stitching: ratatui's `Block` draws ` ` (space) at the top-left corner
    // when only LEFT+TOP borders are set, so we paint the corner glyph
    // ourselves. Then connect the LEFT border to the bottom-bar's top + bot
    // horizontals (┤) and the TOP border to the body area's bottom and to
    // the bottom-bar's right vertical (┴ / ┐).
    let buf = f.buffer_mut();
    let bb_top = term.bottom().saturating_sub(bottom_h);

    // Top-left corner of the art panel — the body area's right border (if
    // any) and the art's LEFT border meet here; the art's TOP border runs
    // rightward from this cell. Use ┌ since there's no border above-left.
    if let Some(c) = buf.cell_mut((art_rect.x, art_rect.y)) {
        c.set_symbol("┌");
        c.set_style(playing_border);
    }

    // Top-right corner: art's TOP border ends here, and the body block's
    // right vertical "│" continues above. Glyph: LEFT + UP = ┘.
    let right = art_rect.x.saturating_add(art_w).saturating_sub(1);
    if right < term.right() {
        if let Some(c) = buf.cell_mut((right, art_rect.y)) {
            c.set_symbol("┘");
            c.set_style(playing_border);
        }
    }

    // Body's bottom border (row bb_top - 1) draws "─" left of art_rect.x;
    // art's LEFT "│" passes through that cell, so stitch ┤ to merge them.
    // No analogous horizontal exists at the bottom-bar's top/bottom rows
    // at this column (bottom_bar was shrunk to stop short of art panel),
    // so don't add notches there — they'd imply phantom horizontals.
    if art_rect.y < bb_top {
        let body_bottom = bb_top.saturating_sub(1);
        if art_rect.y <= body_bottom {
            if let Some(c) = buf.cell_mut((art_rect.x, body_bottom)) {
                c.set_symbol("┤");
                c.set_style(playing_border);
            }
        }
    }
}

/// `0:42 ━━━━●─────────── 4:17` style bar. For radio (no duration) renders an
/// indeterminate scrolling pattern with elapsed-only timestamp.
fn build_progress_bar(
    elapsed: Duration,
    duration: Option<Duration>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let elapsed_str = fmt_mmss(elapsed);
    match duration {
        Some(d) if d.as_secs() > 0 => {
            let total_str = fmt_mmss(d);
            let bar_width = width.saturating_sub(elapsed_str.len() + total_str.len() + 2);
            if bar_width == 0 {
                return Line::from(vec![Span::styled(
                    format!("{elapsed_str} {total_str}"),
                    theme.fg(),
                )]);
            }
            let ratio = (elapsed.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0);
            // Sub-cell precision via 8 horizontal block glyphs. Smoother than
            // a discrete head marker and lines up with adjacent cells cleanly.
            let total_eighths = (bar_width as f64 * 8.0 * ratio).round() as usize;
            let full = total_eighths / 8;
            let part = total_eighths % 8;
            let filled_part: String = "█".repeat(full);
            let partial = match part {
                0 => "",
                1 => "▏",
                2 => "▎",
                3 => "▍",
                4 => "▌",
                5 => "▋",
                6 => "▊",
                _ => "▉",
            };
            let unfilled_w = bar_width.saturating_sub(full + if part > 0 { 1 } else { 0 });
            let unfilled_part: String = "·".repeat(unfilled_w);
            let mut spans = vec![
                Span::styled(format!("{elapsed_str} "), theme.dim()),
                Span::styled(filled_part, theme.progress()),
            ];
            if part > 0 {
                spans.push(Span::styled(partial, theme.progress()));
            }
            spans.push(Span::styled(unfilled_part, theme.progress_track()));
            spans.push(Span::styled(format!(" {total_str}"), theme.dim()));
            Line::from(spans)
        }
        _ => {
            // Indeterminate / live-stream bar: scrolling block pattern.
            let bar_width = width.saturating_sub(elapsed_str.len() + 1);
            if bar_width == 0 {
                return Line::from(vec![Span::styled(elapsed_str, theme.dim())]);
            }
            let offset = (elapsed.as_secs() as usize) % bar_width.max(1);
            let mut bar = String::with_capacity(bar_width * 3);
            for i in 0..bar_width {
                if (i + offset) % 6 < 3 {
                    bar.push('▓');
                } else {
                    bar.push('░');
                }
            }
            Line::from(vec![
                Span::styled(format!("{elapsed_str} "), theme.dim()),
                Span::styled(bar, theme.progress()),
            ])
        }
    }
}

fn fmt_mmss(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Modal help overlay: every binding in the active keymap, plus leader chords.
/// Press `?`, `q`, or `Esc` to close. Centered, themed, dim border.
/// Dedicated lyrics view: takes over the body area while `lyrics_visible`.
/// Synced lyrics center the active line (advanced against playback position)
/// and highlight it; plain lyrics render as a static centered block. Ported
/// from spotatui `player.rs::draw_lyrics`.
fn render_lyrics(app: &App, f: &mut Frame<'_>, area: Rect) {
    use crate::lyrics::LyricsStatus;

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Lyrics ")
        .border_style(app.theme.dim());
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Centered single-line message for the non-rendered states.
    let message = |f: &mut Frame<'_>, msg: &str| {
        let p = Paragraph::new(msg)
            .style(app.theme.dim())
            .alignment(Alignment::Center);
        let r = Rect {
            x: inner.x,
            y: inner.y + inner.height / 2,
            width: inner.width,
            height: 1,
        };
        f.render_widget(p, r);
    };

    let Some(lyr) = app.lyrics.as_ref() else {
        message(f, "No track playing.");
        return;
    };

    match lyr.status {
        LyricsStatus::Loading => message(f, "Loading lyrics..."),
        LyricsStatus::NotFound => message(f, "No lyrics found for this track."),
        LyricsStatus::Plain => {
            // Nothing to sync against — render the whole block centered, wrapped.
            let lines: Vec<Line<'static>> = lyr
                .lines
                .iter()
                .map(|(_, t)| Line::from(Span::styled(t.clone(), app.theme.fg())))
                .collect();
            let p = Paragraph::new(lines)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false });
            f.render_widget(p, inner);
        }
        LyricsStatus::Synced => {
            // Last line whose timestamp has passed is the active one.
            let cur_ms = app
                .playback
                .as_ref()
                .map(|p| p.elapsed.as_millis())
                .unwrap_or(0);
            let mut active = 0usize;
            for (i, (t, _)) in lyr.lines.iter().enumerate() {
                if *t <= cur_ms {
                    active = i;
                } else {
                    break;
                }
            }

            // Anchor the active line to the vertical center, rendering the
            // window of lines around it row by row.
            let mid = (inner.height / 2) as i32;
            for row in 0..inner.height as i32 {
                let idx = active as i32 + row - mid;
                if idx < 0 || idx as usize >= lyr.lines.len() {
                    continue;
                }
                let (_, text) = &lyr.lines[idx as usize];
                let style = if idx as usize == active {
                    app.theme.accent().add_modifier(Modifier::BOLD)
                } else {
                    app.theme.dim()
                };
                let p = Paragraph::new(text.clone())
                    .style(style)
                    .alignment(Alignment::Center);
                let r = Rect {
                    x: inner.x,
                    y: inner.y + row as u16,
                    width: inner.width,
                    height: 1,
                };
                f.render_widget(p, r);
            }
        }
    }
}

fn render_help(app: &App, f: &mut Frame<'_>, area: Rect) {
    use crate::keys::Action;

    let mut bindings: Vec<(String, String)> = app
        .keymap
        .bindings
        .iter()
        .filter_map(|(c, a)| action_label(a).map(|lbl| (chord_label(c), lbl.to_string())))
        .collect();
    bindings.sort_by(|a, b| a.1.cmp(&b.1));

    let mut leaders: Vec<(String, String, String)> = Vec::new();
    for (lc, lm) in &app.keymap.leaders {
        let lk = chord_label(lc);
        for (sub, label, action) in &lm.entries {
            let descr = match action {
                Action::SourceJump(s) => format!("jump → {s}"),
                _ => action_label(action).unwrap_or(label).to_string(),
            };
            leaders.push((format!("{lk} {}", chord_label(sub)), label.clone(), descr));
        }
    }
    leaders.sort_by(|a, b| a.0.cmp(&b.0));

    let key_w = bindings
        .iter()
        .map(|(k, _)| k.len())
        .chain(leaders.iter().map(|(k, _, _)| k.len()))
        .max()
        .unwrap_or(8) as u16
        + 2;

    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled("Bindings", app.theme.header())));
    for (k, action) in &bindings {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{k:<width$}", width = key_w as usize),
                app.theme.accent(),
            ),
            Span::styled(action.clone(), app.theme.fg()),
        ]));
    }
    if !leaders.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Leader chords",
            app.theme.header(),
        )));
        for (k, label, descr) in &leaders {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{k:<width$}", width = key_w as usize),
                    app.theme.accent(),
                ),
                Span::styled(label.clone(), app.theme.fg()),
                Span::raw("  "),
                Span::styled(descr.clone(), app.theme.dim()),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "press ? / q / Esc to close",
        app.theme.dim(),
    )));

    let total_lines = lines.len() as u16;
    // Cap popup at 80% of screen height; keyboard scroll handles overflow.
    let popup_h = (total_lines + 2)
        .min((area.height as f32 * 0.8) as u16)
        .max(6);
    let popup_w: u16 = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(4)
        .min(area.width.saturating_sub(2))
        .max(40);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(" help — j/k/C-d/C-u/g/G ", app.theme.accent()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.leader_border));
    // Inner content rows = popup_h - 2 (top/bottom border). Clamp scroll so
    // we never run past the last line; G (= u16::MAX) collapses here too.
    let inner_h = popup_h.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(inner_h);
    let scroll = app.help_scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        popup,
    );
}

/// Sort axis picker. j/k navigate, Enter applies, Esc/q closes. Per-tab
/// persistence handled in `App::apply_selected_sort`.
fn render_sort_modal(app: &App, f: &mut Frame<'_>, area: Rect) {
    use crate::types::SortAxis;
    let title = " Sort — j/k Enter Esc ";
    let lines: Vec<Line<'_>> = SortAxis::all()
        .iter()
        .enumerate()
        .map(|(i, axis)| {
            let cursor = if i == app.sort_modal_sel { ">" } else { " " };
            let style = if i == app.sort_modal_sel {
                app.theme.selection()
            } else {
                app.theme.fg()
            };
            Line::from(vec![
                Span::styled(format!("{cursor} "), app.theme.accent()),
                Span::styled(axis.label(), style),
            ])
        })
        .collect();
    let popup_w: u16 = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(4)
        .min(area.width.saturating_sub(2))
        .max(40);
    let popup_h = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(4);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(title, app.theme.accent()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.leader_border));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Spotify Connect device picker. Lists active + idle devices; arrow row
/// marks the currently-active one. Enter transfers playback.
fn render_device_modal(app: &App, f: &mut Frame<'_>, area: Rect) {
    let title = " Spotify devices — j/k Enter Esc ";
    let mut lines: Vec<Line<'_>> = Vec::new();
    if app.device_modal_loading {
        lines.push(Line::from(Span::styled(
            "fetching devices…",
            app.theme.dim(),
        )));
    } else if app.devices.is_empty() {
        lines.push(Line::from(Span::styled(
            "no devices visible — open Spotify on phone/desktop first",
            app.theme.dim(),
        )));
    } else {
        for (i, d) in app.devices.iter().enumerate() {
            let cursor = if i == app.device_modal_sel { ">" } else { " " };
            // Three-char active marker so it's visible against the fg theme
            // even on terminals where a single asterisk reads as whitespace.
            let active = if d.is_active { "[*]" } else { "[ ]" };
            let vol = d
                .volume_percent
                .map(|v| format!(" {v}%"))
                .unwrap_or_default();
            let row_style = if i == app.device_modal_sel {
                app.theme.selection()
            } else {
                app.theme.fg()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{cursor} "), app.theme.accent()),
                Span::styled(format!("{active} "), app.theme.accent()),
                Span::styled(d.name.clone(), row_style),
                Span::raw("  "),
                Span::styled(format!("[{}]{vol}", d.kind), app.theme.dim()),
            ]));
        }
    }

    let popup_w: u16 = lines
        .iter()
        .map(|l| l.width() as u16)
        .max()
        .unwrap_or(40)
        .saturating_add(4)
        .min(area.width.saturating_sub(2))
        .max(40);
    let popup_h = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(4);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(Span::styled(title, app.theme.accent()))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.leader_border));
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

fn action_label(a: &crate::keys::Action) -> Option<&'static str> {
    use crate::keys::Action;
    Some(match a {
        Action::Quit => "quit",
        Action::Down => "down",
        Action::Up => "up",
        Action::PageDown => "page down",
        Action::PageUp => "page up",
        Action::Top => "top",
        Action::Bottom => "bottom",
        Action::NextTab => "next tab",
        Action::PrevTab => "prev tab",
        // Tab labels at fixed index would be wrong now (mode-driven tab list
        // means slot 0 differs per mode), so just call them by number.
        Action::TabByIndex(0) => "tab 1",
        Action::TabByIndex(1) => "tab 2",
        Action::TabByIndex(2) => "tab 3",
        Action::TabByIndex(3) => "tab 4",
        Action::TabByIndex(4) => "tab 5",
        Action::TabByIndex(5) => "tab 6",
        Action::TabByIndex(6) => "tab 7",
        Action::TabByIndex(_) => "tab",
        Action::Activate => "activate / play",
        Action::Enqueue => "add to queue",
        Action::Back => "back",
        Action::JumpRoots => "jump to library roots",
        Action::SourceJump(_) => return None,
        Action::PlayPause => "play / pause",
        Action::NextTrack => "next track",
        Action::PrevTrack => "prev track",
        Action::Stop => "stop",
        Action::Refresh => "refresh view",
        Action::ToggleThumb => "toggle thumb mode",
        Action::CycleSource => "cycle source mode",
        Action::VolumeUp => "volume up",
        Action::VolumeDown => "volume down",
        Action::SetVolume(_) => return None,
        Action::FocusSearch => "search",
        Action::FocusCommand => "command bar",
        Action::ToggleHelp => "toggle help",
        Action::ToggleLyrics => "toggle lyrics",
        Action::ToggleLike => "toggle like",
        Action::OpenDevicePicker => "Spotify devices",
        Action::TransferToSelectedDevice => return None,
        Action::SeekToPermille(_) => return None,
        Action::SeekRelative(s) if *s < 0 => "seek -10s",
        Action::SeekRelative(_) => "seek +10s",
        Action::ToggleShuffle => "shuffle",
        Action::CycleRepeat => "repeat",
        Action::OpenSortModal => "sort",
        Action::ApplySelectedSort => return None,
        Action::FollowPlaying => "follow playing",
        Action::ClearQueue => "clear queue",
        Action::RemoveFromQueue => "remove row",
        Action::ExpandHoveredArt => "view art",
        Action::ToggleArtSize => "art size",
        Action::OpenActionMenu => "action menu",
        Action::TogglePinHovered => "pin row",
        Action::FilterInPage => "filter list",
        Action::DownloadHovered => "download YouTube",
        Action::None => return None,
    })
}

fn chord_label(c: &crate::keys::KeyChord) -> String {
    use crossterm::event::KeyCode;
    let base = match c.code {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(ch) => ch.to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "S-Tab".into(),
        other => format!("{other:?}"),
    };
    let mut out = String::new();
    if c.ctrl {
        out.push_str("C-");
    }
    if c.alt {
        out.push_str("A-");
    }
    out.push_str(&base);
    out
}
