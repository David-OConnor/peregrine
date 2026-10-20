//! The main window's panes: the side panels and the terminal, laid out left
//! to right in the order the user drags them into by the grip strip across
//! each one's top.

use egui::{
    Area, CentralPanel, Color32, DragAndDrop, Frame, Id, LayerId, Order, Panel, Rect, RichText,
    Sense, Stroke, Ui, Visuals, pos2, vec2,
};
use shell::{PanelVis, PanelWidths};

use super::{
    BROWSER_COL_WIDTH, CWD_HIST_COL_WIDTH, REMOTE_COL_WIDTH, RIGHT_COL_WIDTH, central_reserve,
    clipboard, cmd_history, dirs, file_browser, remote, scale_panel_fonts, scratchpad, tabs,
    terminal,
};
use crate::state::State;

/// The Clipboard pane's width until it's dragged. Not saved, unlike the
/// others'.
const CLIPBOARD_COL_WIDTH: f32 = 260.;

/// Height of the strip across the top of each pane that the user drags it
/// by. See [grip].
const GRIP_HEIGHT: f32 = 8.;
/// The grip's dots: how many to a row (there are two rows), how far apart
/// their centres are, and their radius.
const GRIP_DOTS: usize = 6;
const GRIP_DOT_GAP: f32 = 4.;
const GRIP_DOT_RADIUS: f32 = 1.;

/// One of the main window's panes. Its show/hide flag is in [PanelVis].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Bookmarks,
    RecentDirs,
    Files,
    Remote,
    Terminal,
    CmdsThisDir,
    RecentCmds,
    Clipboard,
    Scratchpad,
}

impl Pane {
    /// Every pane, left to right as they are until dragged elsewhere.
    pub const DEFAULT_ORDER: [Self; 9] = [
        Self::Bookmarks,
        Self::RecentDirs,
        Self::Files,
        Self::Remote,
        Self::Terminal,
        Self::CmdsThisDir,
        Self::RecentCmds,
        Self::Clipboard,
        Self::Scratchpad,
    ];

    /// The name the state file's `PANE_ORDER` line stores it under: its
    /// [PanelVis] key.
    fn key(self) -> &'static str {
        match self {
            Self::Bookmarks => "bookmarks",
            Self::RecentDirs => "recent_dirs",
            Self::Files => "file_browser",
            Self::Remote => "remote_terminals",
            Self::Terminal => "terminal",
            Self::CmdsThisDir => "recent_cmds_in_dir",
            Self::RecentCmds => "recent_cmds",
            Self::Clipboard => "clipboard",
            Self::Scratchpad => "scratchpad",
        }
    }

    /// Its name on its show/hide toggle.
    pub fn title(self) -> &'static str {
        match self {
            Self::Bookmarks => "Bookmarks",
            Self::RecentDirs => "Recent dirs",
            Self::Files => "Files",
            Self::Remote => "Remote",
            Self::Terminal => "Terminal",
            Self::CmdsThisDir => "Cmds this dir",
            Self::RecentCmds => "Recent cmds",
            Self::Clipboard => "Clipboard",
            Self::Scratchpad => "Scratchpad",
        }
    }

    /// Its show/hide flag.
    pub fn visible_mut(self, vis: &mut PanelVis) -> &mut bool {
        match self {
            Self::Bookmarks => &mut vis.bookmarks,
            Self::RecentDirs => &mut vis.recent_dirs,
            Self::Files => &mut vis.file_browser,
            Self::Remote => &mut vis.remote_terminals,
            Self::Terminal => &mut vis.terminal,
            Self::CmdsThisDir => &mut vis.recent_cmds_in_dir,
            Self::RecentCmds => &mut vis.recent_cmds,
            Self::Clipboard => &mut vis.clipboard,
            Self::Scratchpad => &mut vis.scratchpad,
        }
    }

    fn visible(self, mut vis: PanelVis) -> bool {
        *self.visible_mut(&mut vis)
    }

    /// Its saved width, for the panes that save one. The terminal takes
    /// whatever width the side panes leave.
    fn width_mut(self, widths: &mut PanelWidths) -> Option<&mut Option<f32>> {
        match self {
            Self::Bookmarks => Some(&mut widths.bookmarks),
            Self::RecentDirs => Some(&mut widths.recent_dirs),
            Self::Files => Some(&mut widths.file_browser),
            Self::Remote => Some(&mut widths.remote_terminals),
            Self::CmdsThisDir => Some(&mut widths.recent_cmds_in_dir),
            Self::RecentCmds => Some(&mut widths.recent_cmds),
            Self::Scratchpad => Some(&mut widths.scratchpad),
            Self::Terminal | Self::Clipboard => None,
        }
    }

    /// Its width as a side panel, until one is saved.
    fn default_width(self) -> f32 {
        match self {
            Self::Bookmarks => dirs::BOOKMARK_COL_WIDTH,
            Self::RecentDirs => dirs::RECENT_DIRS_COL_WIDTH,
            Self::Files => BROWSER_COL_WIDTH,
            Self::Remote => REMOTE_COL_WIDTH,
            Self::CmdsThisDir => CWD_HIST_COL_WIDTH,
            Self::RecentCmds => RIGHT_COL_WIDTH,
            Self::Clipboard => CLIPBOARD_COL_WIDTH,
            Self::Scratchpad => scratchpad::COL_WIDTH,
            // Never a side panel.
            Self::Terminal => super::MIN_CENTRAL_WIDTH,
        }
    }

    /// Its egui panel's id, which egui keeps its live width under.
    fn panel_id(self) -> &'static str {
        match self {
            Self::Bookmarks => "bookmarks_panel",
            Self::RecentDirs => "recent_dirs_panel",
            Self::Files => "file_browser_panel",
            Self::Remote => "remote_terminals_panel",
            Self::Terminal => "terminal_panel",
            Self::CmdsThisDir => "cwd_cmd_history_panel",
            Self::RecentCmds => "cmd_history_panel",
            Self::Clipboard => "clipboard_panel",
            Self::Scratchpad => "scratchpad_panel",
        }
    }
}

/// The pane order saved as `keys` (see [order_keys]). Unknown and repeated
/// keys are skipped, and a pane the keys lack (one added since they were
/// saved, or all of them when nothing was) goes after the pane it follows
/// in [Pane::DEFAULT_ORDER].
pub fn order_from_keys(keys: &[String]) -> [Pane; 9] {
    let mut order: Vec<Pane> = Vec::with_capacity(Pane::DEFAULT_ORDER.len());
    for key in keys {
        if let Some(pane) = Pane::DEFAULT_ORDER.into_iter().find(|p| p.key() == key)
            && !order.contains(&pane)
        {
            order.push(pane);
        }
    }
    for (i, pane) in Pane::DEFAULT_ORDER.into_iter().enumerate() {
        if order.contains(&pane) {
            continue;
        }
        let at = Pane::DEFAULT_ORDER[..i]
            .iter()
            .rev()
            .find_map(|prev| order.iter().position(|p| p == prev))
            .map_or(0, |j| j + 1);
        order.insert(at, pane);
    }
    order.try_into().unwrap_or(Pane::DEFAULT_ORDER)
}

/// `order` as the state file's `PANE_ORDER` line stores it.
pub fn order_keys(order: &[Pane]) -> Vec<String> {
    order.iter().map(|p| p.key().to_owned()).collect()
}

/// Move `pane` within `order` to just before `before`, or, with no
/// `before`, just after `after`.
fn move_pane(order: &mut [Pane], pane: Pane, before: Option<Pane>, after: Option<Pane>) {
    let index = |p: Pane| order.iter().position(|&q| q == p);
    let Some(from) = index(pane) else {
        return;
    };
    // Where it ends up, once taken out from where it was.
    let to = match (before.and_then(index), after.and_then(index)) {
        (Some(b), _) if from < b => b - 1,
        (Some(b), _) => b,
        (None, Some(a)) if from < a => a,
        (None, Some(a)) => a + 1,
        (None, None) => return,
    };
    if from < to {
        order[from..=to].rotate_left(1);
    } else {
        order[to..=from].rotate_right(1);
    }
}

/// Draw the visible panes left to right in their order: those before the
/// terminal as left side panels, those after it as right ones, and the
/// terminal in the middle, taking the width they leave. With the terminal
/// hidden, the pane on its left takes that instead (or on its right, when
/// there's none). Then follow a pane being dragged by its grip.
pub(super) fn draw(state: &mut State, ui: &mut Ui) {
    let order = state.ui.pane_order;
    let vis = state.ui.panel_vis;
    let split = order
        .iter()
        .position(|&p| p == Pane::Terminal)
        .unwrap_or(order.len());
    let shown = |p: &&Pane| **p != Pane::Terminal && p.visible(vis);
    let mut left: Vec<Pane> = order[..split].iter().filter(shown).copied().collect();
    let mut right: Vec<Pane> = order[split..].iter().filter(shown).copied().collect();
    let centre = if vis.terminal {
        Some(Pane::Terminal)
    } else {
        left.pop()
            .or_else(|| (!right.is_empty()).then(|| right.remove(0)))
    };

    let mut rects = Vec::with_capacity(order.len());
    for &pane in &left {
        rects.push((pane, side_panel(state, ui, pane, true)));
    }
    // From the window's edge in: the first one added is the rightmost.
    for &pane in right.iter().rev() {
        rects.push((pane, side_panel(state, ui, pane, false)));
    }
    match centre {
        Some(Pane::Terminal) => rects.push((Pane::Terminal, terminal_pane(state, ui))),
        Some(pane) => {
            let gui_font_size = state.gui_font_size;
            let r = CentralPanel::default().show(ui, |ui| {
                scale_panel_fonts(ui, gui_font_size);
                grip(ui, pane);
                contents(state, ui, pane);
            });
            rects.push((pane, r.response.rect));
        }
        None => {
            CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new("Every pane is hidden. Show one with the toggles above.")
                            .italics()
                            .weak(),
                    );
                });
            });
        }
    }

    follow_drag(state, ui, &rects);
}

/// `pane` as a resizable side panel, on the left or right, at its saved
/// width (falling back to its default). Records the width it was drawn at,
/// so `save` persists drags. Returns where it was drawn.
fn side_panel(state: &mut State, ui: &mut Ui, pane: Pane, left: bool) -> Rect {
    let width = pane
        .width_mut(&mut state.ui.panel_widths)
        .and_then(|w| *w)
        .unwrap_or_else(|| pane.default_width());
    let panel = if left {
        Panel::left(pane.panel_id())
    } else {
        Panel::right(pane.panel_id())
    };
    let gui_font_size = state.gui_font_size;
    let max = central_reserve(ui, f32::INFINITY);
    let r = panel
        .resizable(true)
        .default_size(width)
        .max_size(max)
        .show(ui, |ui| {
            // The side panels' text follows the "GUI font size" control.
            scale_panel_fonts(ui, gui_font_size);
            grip(ui, pane);
            contents(state, ui, pane);
        });
    if let Some(w) = pane.width_mut(&mut state.ui.panel_widths) {
        *w = Some(r.response.rect.width());
    }
    r.response.rect
}

/// What's in `pane`, under its grip.
fn contents(state: &mut State, ui: &mut Ui, pane: Pane) {
    match pane {
        Pane::Bookmarks => dirs::dir_bookmarks(state, ui),
        Pane::RecentDirs => dirs::dir_history(state, ui),
        Pane::Files => file_browser::file_browser(state, ui),
        Pane::Remote => remote::remote_terminals(state, ui),
        Pane::CmdsThisDir => cmd_history::cwd_cmd_history(state, ui),
        Pane::RecentCmds => cmd_history::cmd_history(state, ui),
        Pane::Clipboard => clipboard::draw(state, ui),
        Pane::Scratchpad => scratchpad::draw(state, ui),
        // Only ever in the middle, where [terminal_pane] draws it.
        Pane::Terminal => {}
    }
}

/// The terminal: the input row pinned to the bottom of the middle of the
/// window, and the scrolling output filling the rest above it. Returns where
/// the two were drawn.
fn terminal_pane(state: &mut State, ui: &mut Ui) -> Rect {
    // Black backgrounds on the terminal panes so they read as a terminal,
    // not as a regular widget area.
    let term_frame = Frame::default()
        .fill(Color32::BLACK)
        .inner_margin(egui::Margin::same(6));

    // The terminal panes look the same in either theme: always the light
    // visuals, whose white input box stands out against the black fill (the
    // dark theme's near-black one would vanish into it).
    let input = Panel::bottom("term_in_panel")
        .resizable(false)
        .frame(term_frame)
        .show(ui, |ui| {
            *ui.visuals_mut() = Visuals::light();
            terminal::term_in(state, ui);
        });

    let output = CentralPanel::default().frame(term_frame).show(ui, |ui| {
        // The grip, though, is on the black fill as on a dark background.
        ui.scope(|ui| {
            *ui.visuals_mut() = Visuals::dark();
            grip(ui, Pane::Terminal);
        });
        *ui.visuals_mut() = Visuals::light();
        terminal::term_out(state, ui);
    });

    output.response.rect.union(input.response.rect)
}

/// The strip across the top of `pane` that the user drags it by to move it
/// (see [follow_drag]): two rows of dots, brighter while hovered.
fn grip(ui: &mut Ui, pane: Pane) {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), GRIP_HEIGHT), Sense::drag());
    resp.dnd_set_drag_payload(pane);
    let resp = resp.on_hover_text(format!("Drag to move {}", pane.title()));

    let active = resp.hovered() || resp.dragged();
    let visuals = ui.visuals();
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 2., visuals.widgets.hovered.weak_bg_fill);
    }
    let color = if active {
        visuals.strong_text_color()
    } else {
        visuals.weak_text_color()
    };
    let centre = rect.center();
    let first = centre.x - (GRIP_DOTS - 1) as f32 * GRIP_DOT_GAP / 2.;
    for dy in [-GRIP_DOT_GAP / 2., GRIP_DOT_GAP / 2.] {
        for i in 0..GRIP_DOTS {
            let x = first + i as f32 * GRIP_DOT_GAP;
            painter.circle_filled(pos2(x, centre.y + dy), GRIP_DOT_RADIUS, color);
        }
    }
}

/// While a pane is dragged by its grip: a line where letting go would put
/// it, between the other panes (`rects`, as drawn this frame), unless that's
/// where it is, and its name by the pointer. Once let go, move it there.
fn follow_drag(state: &mut State, ui: &Ui, rects: &[(Pane, Rect)]) {
    let ctx = ui.ctx();
    let Some(pane) = DragAndDrop::payload::<Pane>(ctx).map(|p| *p) else {
        return;
    };
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    let Some(&(_, dragged)) = rects.iter().find(|(p, _)| *p == pane) else {
        return;
    };

    let mut others: Vec<(Pane, Rect)> = rects.iter().filter(|(p, _)| *p != pane).copied().collect();
    others.sort_by(|a, b| a.1.left().total_cmp(&b.1.left()));
    let n = others.len();
    // The gap the pointer is nearest: 0 is before the first of the others,
    // `n` after the last.
    let slot = others.iter().filter(|(_, r)| r.center().x < pos.x).count();
    let here = others
        .iter()
        .filter(|(_, r)| r.center().x < dragged.center().x)
        .count();

    if ctx.input(|i| i.pointer.any_released()) {
        DragAndDrop::clear_payload(ctx);
        if slot != here {
            let before = others.get(slot).map(|o| o.0);
            let after = slot.checked_sub(1).and_then(|i| others.get(i)).map(|o| o.0);
            move_pane(&mut state.ui.pane_order, pane, before, after);
            let _ = state.save();
        }
        return;
    }

    let painter = ctx.layer_painter(LayerId::new(Order::Foreground, Id::new("pane_drop")));
    painter.rect_filled(dragged, 0., tabs::TAB_ACTIVE_FILL.gamma_multiply(0.15));
    if slot != here {
        let half = tabs::TAB_DROP_MARKER_WIDTH / 2.;
        let x = match slot {
            0 => others[0].1.left() + half,
            s if s == n => others[n - 1].1.right() - half,
            s => (others[s - 1].1.right() + others[s].1.left()) / 2.,
        };
        let y = rects
            .iter()
            .fold(dragged, |all, (_, r)| all.union(*r))
            .y_range();
        let stroke = Stroke::new(tabs::TAB_DROP_MARKER_WIDTH, tabs::TAB_ACTIVE_FILL);
        painter.vline(x, y, stroke);
    }

    Area::new(Id::new("pane_drag_preview"))
        .order(Order::Tooltip)
        .interactable(false)
        .fixed_pos(pos + vec2(16., 16.))
        .show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(pane.title());
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(order: &[Pane]) -> Vec<&'static str> {
        order.iter().map(|p| p.key()).collect()
    }

    #[test]
    fn order_from_nothing_is_the_default() {
        assert_eq!(order_from_keys(&[]), Pane::DEFAULT_ORDER);
    }

    #[test]
    fn order_round_trips() {
        let mut order = Pane::DEFAULT_ORDER;
        order.reverse();
        assert_eq!(order_from_keys(&order_keys(&order)), order);
    }

    #[test]
    fn order_skips_unknown_and_repeated_keys_and_fills_in_missing_panes() {
        let saved: Vec<String> = ["terminal", "nope", "bookmarks", "terminal", "clipboard"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            keys(&order_from_keys(&saved)),
            [
                "terminal",
                "recent_cmds_in_dir",
                "recent_cmds",
                "bookmarks",
                "recent_dirs",
                "file_browser",
                "remote_terminals",
                "clipboard",
                "scratchpad",
            ]
        );
    }

    #[test]
    fn moves_before_and_after() {
        use Pane::*;
        let mut order = [Bookmarks, RecentDirs, Files, Terminal];

        move_pane(&mut order, Bookmarks, Some(Terminal), None);
        assert_eq!(order, [RecentDirs, Files, Bookmarks, Terminal]);

        move_pane(&mut order, Terminal, Some(RecentDirs), None);
        assert_eq!(order, [Terminal, RecentDirs, Files, Bookmarks]);

        move_pane(&mut order, Terminal, None, Some(Bookmarks));
        assert_eq!(order, [RecentDirs, Files, Bookmarks, Terminal]);

        move_pane(&mut order, Bookmarks, None, Some(RecentDirs));
        assert_eq!(order, [RecentDirs, Bookmarks, Files, Terminal]);
    }
}
