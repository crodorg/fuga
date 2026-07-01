//! Per-source column schemes for the track-row layout. Shared by the
//! column-header bar and the row renderer (both in `thumb_list`) so the two
//! always split identically and stay aligned.

use ratatui::layout::Constraint;

/// Which `TrackColumns` field a variable-width column pulls from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ColField {
    Artist,
    Title,
    Album,
}

/// A variable-width column: which field it shows, its header label, and how
/// it claims horizontal space. The right-aligned fixed duration column is
/// not modelled here — it's appended by `ColumnLayout::duration`.
#[derive(Clone)]
pub struct Col {
    pub field: ColField,
    pub label: &'static str,
    /// `Some(pct)` → `Constraint::Percentage`; `None` → `Constraint::Fill(1)`
    /// (claim all remaining width). Fill lets one column stretch edge-to-edge
    /// (podcast title, radio station name).
    pub pct: Option<u16>,
}

/// Per-source column scheme shared by the header bar and the row renderer so
/// the two always align. `cols` are the variable-width columns in display
/// order; `duration` appends the fixed 6-cell right-aligned `Time` column.
#[derive(Clone)]
pub struct ColumnLayout {
    pub cols: Vec<Col>,
    pub duration: bool,
}

impl ColumnLayout {
    /// Artist | Song | Album | Time — local, Spotify tracks, the queue.
    pub fn standard() -> Self {
        Self {
            cols: vec![
                Col {
                    field: ColField::Artist,
                    label: "Artist",
                    pct: Some(30),
                },
                Col {
                    field: ColField::Title,
                    label: "Song",
                    pct: Some(35),
                },
                Col {
                    field: ColField::Album,
                    label: "Album",
                    pct: Some(30),
                },
            ],
            duration: true,
        }
    }

    /// Artist | Song | Time — YouTube when no album metadata is present; the
    /// two text columns widen to reclaim the dropped Album column.
    pub fn no_album() -> Self {
        Self {
            cols: vec![
                Col {
                    field: ColField::Artist,
                    label: "Artist",
                    pct: Some(40),
                },
                Col {
                    field: ColField::Title,
                    label: "Song",
                    pct: None,
                },
            ],
            duration: true,
        }
    }

    /// Podcast | Time — episode name fills icon→Time.
    pub fn podcast() -> Self {
        Self {
            cols: vec![Col {
                field: ColField::Title,
                label: "Podcast",
                pct: None,
            }],
            duration: true,
        }
    }

    /// Artist(dj) | Radio(station) | Genre | Time — SomaFM. Time renders `—`
    /// (streams have no length) but the column is kept for alignment.
    pub fn somafm() -> Self {
        Self {
            cols: vec![
                Col {
                    field: ColField::Artist,
                    label: "Artist",
                    pct: Some(30),
                },
                Col {
                    field: ColField::Title,
                    label: "Radio",
                    pct: Some(35),
                },
                Col {
                    field: ColField::Album,
                    label: "Genre",
                    pct: Some(30),
                },
            ],
            duration: true,
        }
    }

    /// Radio — single full-width station-name column, no Time.
    pub fn radio() -> Self {
        Self {
            cols: vec![Col {
                field: ColField::Title,
                label: "Radio",
                pct: None,
            }],
            duration: false,
        }
    }

    /// The ratatui constraints for `area`, shared by header + rows so both
    /// split identically: one per variable column, then (if `duration`) the
    /// 6-cell Time column and a 1-cell trailing spacer.
    pub(crate) fn constraints(&self) -> Vec<Constraint> {
        let mut c: Vec<Constraint> = self
            .cols
            .iter()
            .map(|col| match col.pct {
                Some(p) => Constraint::Percentage(p),
                None => Constraint::Fill(1),
            })
            .collect();
        if self.duration {
            c.push(Constraint::Length(6));
            c.push(Constraint::Length(1));
        }
        c
    }
}
