//! ZTab switcher panel: a non-activating HUD panel that shows window tiles in a
//! wrapping, app-grouped grid. Everything here is main-thread only.
//!
//! Structure:
//! - `compute_layout` / `Layout` are pure geometry (no AppKit), unit tested below.
//! - `SwitcherView` is one flipped custom NSView that draws every tile in `drawRect:`
//!   and maps mouse events to tile indices through `Layout::hit_test`.
//! - `Switcher` owns the NSPanel (content = NSVisualEffectView, HUD material, rounded
//!   via a 9-slice mask image) and the view.
#![allow(dead_code)]

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSBitmapImageFileType, NSColor,
    NSCompositingOperation, NSEvent, NSFont, NSFontAttributeName, NSFontWeightMedium,
    NSFontWeightRegular, NSFontWeightSemibold, NSForegroundColorAttributeName, NSGraphicsContext,
    NSImage, NSImageInterpolation, NSImageResizingMode, NSLineBreakMode, NSMutableParagraphStyle,
    NSPanel, NSParagraphStyleAttributeName, NSPopUpMenuWindowLevel, NSResponder, NSScreen,
    NSStringDrawingOptions, NSStringNSExtendedStringDrawing, NSStringDrawing, NSTrackingArea,
    NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowAnimationBehavior,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{
    NSAttributedStringKey, NSDictionary, NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};

// ---------------------------------------------------------------------------
// Public data
// ---------------------------------------------------------------------------

/// One window tile. Tiles of the same app must be adjacent and share `group`.
#[derive(Clone, Debug, Default)]
pub struct Tile {
    pub app_name: String,
    pub title: String,
    /// App icon.
    pub icon: Option<Retained<NSImage>>,
    pub thumbnail: Option<Retained<NSImage>>,
    /// Window width / height (sizes the thumbnail area before the image arrives).
    pub aspect: f64,
    pub minimized: bool,
    /// App group index; consecutive tiles share a group.
    pub group: usize,
}

// ---------------------------------------------------------------------------
// Pure layout (flipped coordinates: origin top-left of the panel)
// ---------------------------------------------------------------------------

/// Padding between the panel edge and the grid.
pub const PANEL_PAD: f64 = 16.0;
/// Panel corner radius.
pub const CORNER_RADIUS: f64 = 18.0;
/// Padding inside a group background around its tiles.
pub const GROUP_PAD: f64 = 5.0;
/// Horizontal gap between two groups in a row.
pub const GROUP_GAP: f64 = 10.0;
/// Horizontal gap between tiles of the same group.
pub const TILE_GAP: f64 = 2.0;
/// Vertical gap between rows.
pub const ROW_GAP: f64 = 10.0;
/// Padding inside a tile around the thumbnail.
pub const TILE_PAD: f64 = 8.0;
/// Gap between thumbnail and the label row.
pub const LABEL_GAP: f64 = 6.0;
/// Height of the icon + title row.
pub const LABEL_H: f64 = 20.0;
/// Padding below the label row.
pub const TILE_PAD_BOTTOM: f64 = 6.0;
/// Small app icon size in the label row.
pub const SMALL_ICON: f64 = 18.0;
/// Preferred thumbnail height.
pub const BASE_THUMB_H: f64 = 140.0;
/// Smallest thumbnail height we shrink to before giving up on fitting vertically.
pub const MIN_THUMB_H: f64 = 26.0;
/// Thumbnail width clamp, as a multiple of thumbnail height.
pub const MIN_ASPECT: f64 = 1.0;
pub const MAX_ASPECT: f64 = 1.78;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Rect { x, y, w, h }
    }
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    pub fn cx(&self) -> f64 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f64 {
        self.y + self.h / 2.0
    }
    pub fn inset(&self, d: f64) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x < o.x + o.w && o.x < self.x + self.w && self.y < o.y + o.h && o.y < self.y + self.h
    }
    /// Largest rect of aspect `a` (w/h) centered inside `self`.
    pub fn fit(&self, a: f64) -> Rect {
        let a = if a.is_finite() && a > 0.0 { a } else { 1.0 };
        let (w, h) = if self.w / self.h > a { (self.h * a, self.h) } else { (self.w, self.w / a) };
        Rect::new(self.x + (self.w - w) / 2.0, self.y + (self.h - h) / 2.0, w, h)
    }
    fn ns(&self) -> NSRect {
        NSRect::new(NSPoint::new(self.x, self.y), NSSize::new(self.w, self.h))
    }
    fn from_ns(r: NSRect) -> Rect {
        Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
    }
}

/// Result of `compute_layout`. All rects are in the panel's flipped coordinate space.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// Panel content size.
    pub width: f64,
    pub height: f64,
    /// Thumbnail area height actually used (<= BASE_THUMB_H).
    pub thumb_h: f64,
    /// One rect per tile, same order as the input.
    pub tiles: Vec<Rect>,
    /// Row index of each tile.
    pub tile_row: Vec<usize>,
    /// Tile indices per row, left to right.
    pub rows: Vec<Vec<usize>>,
    /// Group background rects (one per run of a group within a row) with the group id.
    pub groups: Vec<(Rect, usize)>,
}

fn tile_w(aspect: f64, th: f64) -> f64 {
    let a = if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.6 };
    (th * a.clamp(MIN_ASPECT, MAX_ASPECT)).round() + 2.0 * TILE_PAD
}

fn tile_h(th: f64) -> f64 {
    TILE_PAD + th + LABEL_GAP + LABEL_H + TILE_PAD_BOTTOM
}

/// Rows -> segments (runs of one group within a row) -> tile indices.
type Rows = Vec<Vec<Vec<usize>>>;

fn seg_width(items: &[(f64, usize)], seg: &[usize], th: f64) -> f64 {
    let tiles: f64 = seg.iter().map(|&i| tile_w(items[i].0, th)).sum();
    2.0 * GROUP_PAD + tiles + TILE_GAP * (seg.len().saturating_sub(1)) as f64
}

fn row_width(items: &[(f64, usize)], row: &[Vec<usize>], th: f64) -> f64 {
    let segs: f64 = row.iter().map(|s| seg_width(items, s, th)).sum();
    segs + GROUP_GAP * (row.len().saturating_sub(1)) as f64
}

fn break_rows(items: &[(f64, usize)], th: f64, avail_w: f64) -> Rows {
    let mut rows: Rows = Vec::new();
    let mut cur: Vec<Vec<usize>> = Vec::new();
    let mut cur_w = 0.0;
    let mut i = 0;
    while i < items.len() {
        let g = items[i].1;
        let mut end = i;
        while end < items.len() && items[end].1 == g {
            end += 1;
        }
        let run: Vec<usize> = (i..end).collect();
        let gw = seg_width(items, &run, th);
        // A group that does not fit in what is left of the row starts a new row, so it
        // is never split when it fits in one row. Groups wider than a row are split.
        if !cur.is_empty() && cur_w + GROUP_GAP + gw > avail_w {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0.0;
        }
        let mut seg: Vec<usize> = Vec::new();
        let mut seg_w = 2.0 * GROUP_PAD;
        for idx in run {
            let tw = tile_w(items[idx].0, th);
            let add = if seg.is_empty() { tw } else { TILE_GAP + tw };
            let prefix = if cur.is_empty() { 0.0 } else { cur_w + GROUP_GAP };
            if !(seg.is_empty() && cur.is_empty()) && prefix + seg_w + add > avail_w {
                if !seg.is_empty() {
                    cur.push(std::mem::take(&mut seg));
                }
                rows.push(std::mem::take(&mut cur));
                cur_w = 0.0;
                seg_w = 2.0 * GROUP_PAD + tw;
                seg.push(idx);
                continue;
            }
            seg.push(idx);
            seg_w += add;
        }
        if !seg.is_empty() {
            cur_w = if cur.is_empty() { seg_w } else { cur_w + GROUP_GAP + seg_w };
            cur.push(seg);
        }
        i = end;
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Lay out tiles given `(aspect, group)` per tile, within a panel of at most
/// `max_w` x `max_h` points. Shrinks the thumbnail height until everything fits
/// vertically (down to `MIN_THUMB_H`).
pub fn compute_layout(items: &[(f64, usize)], max_w: f64, max_h: f64) -> Layout {
    if items.is_empty() {
        return Layout {
            width: 2.0 * PANEL_PAD,
            height: 2.0 * PANEL_PAD,
            thumb_h: BASE_THUMB_H,
            ..Default::default()
        };
    }
    let avail_w = (max_w - 2.0 * PANEL_PAD).max(1.0);
    let mut th = BASE_THUMB_H;
    let (rows, row_h) = loop {
        let rows = break_rows(items, th, avail_w);
        let row_h = tile_h(th) + 2.0 * GROUP_PAD;
        let n = rows.len() as f64;
        let h = 2.0 * PANEL_PAD + n * row_h + (n - 1.0) * ROW_GAP;
        if h <= max_h || th <= MIN_THUMB_H {
            break (rows, row_h);
        }
        th = (th * 0.93).floor().max(MIN_THUMB_H);
    };

    let widths: Vec<f64> = rows.iter().map(|r| row_width(items, r, th)).collect();
    let content_w = widths.iter().cloned().fold(0.0, f64::max);
    let n = rows.len();
    let mut out = Layout {
        width: (content_w + 2.0 * PANEL_PAD).ceil(),
        height: (2.0 * PANEL_PAD + n as f64 * row_h + (n as f64 - 1.0) * ROW_GAP).ceil(),
        thumb_h: th,
        tiles: vec![Rect::default(); items.len()],
        tile_row: vec![0; items.len()],
        rows: Vec::with_capacity(n),
        groups: Vec::new(),
    };
    let th_tile = tile_h(th);
    for (r, row) in rows.iter().enumerate() {
        let y = PANEL_PAD + r as f64 * (row_h + ROW_GAP);
        let mut x = (PANEL_PAD + (content_w - widths[r]) / 2.0).round();
        let mut flat = Vec::new();
        for seg in row {
            let sw = seg_width(items, seg, th);
            out.groups.push((Rect::new(x, y, sw, row_h), items[seg[0]].1));
            let mut tx = x + GROUP_PAD;
            for &i in seg {
                let tw = tile_w(items[i].0, th);
                out.tiles[i] = Rect::new(tx, y + GROUP_PAD, tw, th_tile);
                out.tile_row[i] = r;
                flat.push(i);
                tx += tw + TILE_GAP;
            }
            x += sw + GROUP_GAP;
        }
        out.rows.push(flat);
    }
    out
}

impl Layout {
    /// Tile index under the point (flipped panel coordinates), if any.
    pub fn hit_test(&self, x: f64, y: f64) -> Option<usize> {
        self.tiles.iter().position(|r| r.contains(x, y))
    }

    /// Tile in the row above (`dir = -1`) or below (`dir = 1`) whose horizontal
    /// center is closest to that of `idx`. Clamps: returns `idx` on the first/last row.
    pub fn vertical_neighbor(&self, idx: usize, dir: isize) -> usize {
        if idx >= self.tiles.len() {
            return idx.min(self.tiles.len().saturating_sub(1));
        }
        let row = self.tile_row[idx] as isize + dir;
        if row < 0 || row as usize >= self.rows.len() {
            return idx;
        }
        let cx = self.tiles[idx].cx();
        self.rows[row as usize]
            .iter()
            .copied()
            .min_by(|&a, &b| {
                let da = (self.tiles[a].cx() - cx).abs();
                let db = (self.tiles[b].cx() - cx).abs();
                da.partial_cmp(&db).unwrap()
            })
            .unwrap_or(idx)
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ViewState {
    tiles: Vec<Tile>,
    layout: Layout,
    selected: usize,
    hovered: Option<usize>,
    pressed: Option<usize>,
    /// Paint an approximate HUD backdrop (cacheDisplay cannot capture the blur).
    snapshot_backdrop: bool,
}

pub struct ViewIvars {
    state: RefCell<ViewState>,
    on_click: Rc<dyn Fn(usize)>,
    on_hover: Rc<dyn Fn(usize)>,
}

impl fmt::Debug for ViewIvars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ViewIvars")
    }
}

define_class!(
    // SAFETY: NSView has no subclassing requirements beyond main-thread use;
    // SwitcherView does not implement Drop.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ZTabSwitcherView"]
    #[ivars = ViewIvars]
    pub struct SwitcherView;

    unsafe impl NSObjectProtocol for SwitcherView {}

    impl SwitcherView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw(Rect::from_ns(dirty));
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let idx = self.index_at(event);
            let fire = {
                let mut st = self.ivars().state.borrow_mut();
                if idx.is_some() && idx != st.hovered {
                    st.hovered = idx;
                    true
                } else {
                    if idx.is_none() {
                        st.hovered = None;
                    }
                    false
                }
            };
            if let (true, Some(i)) = (fire, idx) {
                let cb = self.ivars().on_hover.clone();
                cb(i);
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.ivars().state.borrow_mut().hovered = None;
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let idx = self.index_at(event);
            self.ivars().state.borrow_mut().pressed = idx;
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let idx = self.index_at(event);
            let pressed = self.ivars().state.borrow_mut().pressed.take();
            // Commit on mouse-up over the same tile that got the mouse-down.
            if let Some(i) = idx.filter(|&i| pressed == Some(i)) {
                let cb = self.ivars().on_click.clone();
                cb(i);
            }
        }
    }
);

fn ns_rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn rounded(r: Rect, radius: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(r.ns(), radius, radius)
}

fn draw_image(img: &NSImage, r: Rect, alpha: f64) {
    // SAFETY: plain drawing call, no hints dictionary.
    unsafe {
        img.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            r.ns(),
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            alpha,
            true,
            None,
        )
    };
}

fn text_attrs(
    font: &NSFont,
    color: &NSColor,
    truncate: bool,
) -> Retained<NSDictionary<NSAttributedStringKey, AnyObject>> {
    let para = NSMutableParagraphStyle::new();
    if truncate {
        para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    }
    let f: &AnyObject = font;
    let c: &AnyObject = color;
    let p: &AnyObject = &para;
    // SAFETY: the attribute-name statics are provided by AppKit.
    let keys: [&NSAttributedStringKey; 3] = unsafe {
        [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName]
    };
    NSDictionary::from_slices(&keys, &[f, c, p])
}

fn draw_text(s: &NSString, r: Rect, attrs: &NSDictionary<NSAttributedStringKey, AnyObject>) {
    // SAFETY: attrs has NSFont / NSColor / NSParagraphStyle values for the matching keys.
    unsafe {
        s.drawWithRect_options_attributes_context(
            r.ns(),
            NSStringDrawingOptions::UsesLineFragmentOrigin
                | NSStringDrawingOptions::TruncatesLastVisibleLine,
            Some(attrs),
            None,
        )
    };
}

impl SwitcherView {
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        on_click: Rc<dyn Fn(usize)>,
        on_hover: Rc<dyn Fn(usize)>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            state: RefCell::new(ViewState::default()),
            on_click,
            on_hover,
        });
        // SAFETY: NSView's designated initializer.
        let view: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        let opts = NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::InVisibleRect;
        let owner: &AnyObject = &view;
        // SAFETY: owner is this view, which outlives the tracking area it owns.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                opts,
                Some(owner),
                None,
            )
        };
        view.addTrackingArea(&area);
        view
    }

    fn index_at(&self, event: &NSEvent) -> Option<usize> {
        let p = self.convertPoint_fromView(event.locationInWindow(), None);
        self.ivars().state.borrow().layout.hit_test(p.x, p.y)
    }

    fn invalidate_tile(&self, st: &ViewState, idx: usize) {
        if let Some(r) = st.layout.tiles.get(idx) {
            self.setNeedsDisplayInRect(r.inset(-2.0).ns());
        }
    }

    fn draw(&self, dirty: Rect) {
        let st = self.ivars().state.borrow();
        if let Some(ctx) = NSGraphicsContext::currentContext() {
            ctx.setImageInterpolation(NSImageInterpolation::Medium);
        }
        let bounds = Rect::from_ns(self.bounds());

        // Tint over the HUD material: keeps text contrast when the material is very
        // translucent (e.g. dark appearance over a bright desktop). The snapshot uses a
        // near-opaque backdrop because cacheDisplay cannot capture the blur.
        let tint = if st.snapshot_backdrop { 0.96 } else { 0.5 };
        NSColor::windowBackgroundColor().colorWithAlphaComponent(tint).setFill();
        rounded(bounds, CORNER_RADIUS).fill();
        // Hairline edge, like system HUDs.
        let edge = rounded(bounds.inset(0.5), CORNER_RADIUS - 0.5);
        edge.setLineWidth(1.0);
        NSColor::labelColor().colorWithAlphaComponent(0.12).setStroke();
        edge.stroke();

        // Group backgrounds.
        let group_fill = NSColor::labelColor().colorWithAlphaComponent(0.055);
        let group_edge = NSColor::labelColor().colorWithAlphaComponent(0.07);
        for (g, _) in &st.layout.groups {
            if !g.intersects(&dirty) {
                continue;
            }
            let p = rounded(*g, 12.0);
            group_fill.setFill();
            p.fill();
            let e = rounded(g.inset(0.5), 11.5);
            e.setLineWidth(1.0);
            group_edge.setStroke();
            e.stroke();
        }

        let title_font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightRegular });
        let sel_font = NSFont::systemFontOfSize_weight(12.0, unsafe { NSFontWeightMedium });
        let label = NSColor::labelColor();
        let attrs = text_attrs(&title_font, &label, true);
        let sel_attrs = text_attrs(&sel_font, &label, true);
        let line_h = (title_font.ascender() - title_font.descender() + title_font.leading()).ceil();

        for (i, tile) in st.tiles.iter().enumerate() {
            let Some(&r) = st.layout.tiles.get(i) else { break };
            if !r.inset(-3.0).intersects(&dirty) {
                continue;
            }
            let selected = i == st.selected;
            let th = st.layout.thumb_h;

            if selected {
                let accent = NSColor::controlAccentColor();
                accent.colorWithAlphaComponent(0.22).setFill();
                rounded(r, 10.0).fill();
                let ring = rounded(r.inset(1.5), 8.5);
                ring.setLineWidth(3.0);
                accent.setStroke();
                ring.stroke();
            }

            let area = Rect::new(r.x + TILE_PAD, r.y + TILE_PAD, r.w - 2.0 * TILE_PAD, th);
            let alpha = if tile.minimized { 0.45 } else { 1.0 };
            let content = match &tile.thumbnail {
                Some(img) => {
                    let sz = img.size();
                    let a = if sz.height > 0.0 { sz.width / sz.height } else { tile.aspect };
                    let fr = area.fit(a);
                    NSGraphicsContext::saveGraphicsState_class();
                    rounded(fr, 6.0).addClip();
                    draw_image(img, fr, alpha);
                    NSGraphicsContext::restoreGraphicsState_class();
                    let e = rounded(fr.inset(0.25), 6.0);
                    e.setLineWidth(0.5);
                    NSColor::labelColor().colorWithAlphaComponent(0.18).setStroke();
                    e.stroke();
                    fr
                }
                None => {
                    let a = tile.aspect.clamp(MIN_ASPECT, MAX_ASPECT);
                    let fr = area.fit(if a.is_finite() { a } else { 1.6 });
                    NSColor::labelColor().colorWithAlphaComponent(0.06).setFill();
                    rounded(fr, 6.0).fill();
                    if let Some(icon) = &tile.icon {
                        let s = (fr.w.min(fr.h) * 0.56).min(96.0).round();
                        let ir = Rect::new(
                            (fr.cx() - s / 2.0).round(),
                            (fr.cy() - s / 2.0).round(),
                            s,
                            s,
                        );
                        draw_image(icon, ir, if tile.minimized { 0.55 } else { 1.0 });
                    }
                    fr
                }
            };

            if tile.minimized {
                self.draw_badge(content);
            }

            // Label row: small icon + title.
            let ly = r.y + TILE_PAD + th + LABEL_GAP;
            let mut tx = r.x + TILE_PAD;
            if let Some(icon) = &tile.icon {
                let ir = Rect::new(tx, (ly + (LABEL_H - SMALL_ICON) / 2.0).round(), SMALL_ICON, SMALL_ICON);
                draw_image(icon, ir, 1.0);
                tx += SMALL_ICON + 5.0;
            }
            let text = if tile.title.trim().is_empty() { &tile.app_name } else { &tile.title };
            let s = NSString::from_str(text);
            let tr = Rect::new(tx, ly + ((LABEL_H - line_h) / 2.0).round(), r.x + r.w - TILE_PAD - tx, line_h);
            let a = if selected {
                &sel_attrs
            } else {
                &attrs
            };
            draw_text(&s, tr, a);
        }
    }

    fn draw_badge(&self, content: Rect) {
        let font = NSFont::systemFontOfSize_weight(10.0, unsafe { NSFontWeightSemibold });
        let white = NSColor::whiteColor();
        let attrs = text_attrs(&font, &white, false);
        let s = NSString::from_str("Minimized");
        // SAFETY: attrs is a valid attribute dictionary.
        let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
        let h = (sz.height + 4.0).round();
        let w = (sz.width + 14.0).round();
        if w > content.w - 8.0 {
            return;
        }
        let br = Rect::new((content.cx() - w / 2.0).round(), content.y + content.h - h - 6.0, w, h);
        NSColor::colorWithWhite_alpha(0.0, 0.62).setFill();
        rounded(br, h / 2.0).fill();
        let tr = Rect::new(br.x + 7.0, br.y + (h - sz.height) / 2.0, sz.width + 1.0, sz.height);
        // SAFETY: attrs is a valid attribute dictionary.
        unsafe { s.drawInRect_withAttributes(tr.ns(), Some(&attrs)) };
    }
}

// ---------------------------------------------------------------------------
// The switcher
// ---------------------------------------------------------------------------

/// The switcher panel. Created once and reused. Main thread only (not Send/Sync).
pub struct Switcher {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    effect: Retained<NSVisualEffectView>,
    view: Retained<SwitcherView>,
}

fn mask_image(radius: f64) -> Retained<NSImage> {
    let block = RcBlock::new(move |rect: NSRect| -> Bool {
        NSColor::blackColor().setFill();
        NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius).fill();
        Bool::YES
    });
    let side = 2.0 * radius + 1.0;
    let img = NSImage::imageWithSize_flipped_drawingHandler(NSSize::new(side, side), false, &block);
    img.setCapInsets(NSEdgeInsets { top: radius, left: radius, bottom: radius, right: radius });
    img.setResizingMode(NSImageResizingMode(1)); // NSImageResizingModeStretch
    img
}

/// Screen containing the mouse cursor, else the main screen.
fn target_screen(mtm: MainThreadMarker) -> Option<Retained<NSScreen>> {
    let m = NSEvent::mouseLocation();
    for s in NSScreen::screens(mtm).iter() {
        let f = s.frame();
        if m.x >= f.origin.x
            && m.x <= f.origin.x + f.size.width
            && m.y >= f.origin.y
            && m.y <= f.origin.y + f.size.height
        {
            return Some(s);
        }
    }
    NSScreen::mainScreen(mtm)
}

impl Switcher {
    pub fn new(mtm: MainThreadMarker, on_click: Box<dyn Fn(usize)>, on_hover: Box<dyn Fn(usize)>) -> Self {
        let frame = ns_rect(0.0, 0.0, 400.0, 200.0);
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // SAFETY: we own the panel through Retained and never rely on close-release.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setFloatingPanel(true);
        panel.setLevel(NSPopUpMenuWindowLevel);
        panel.setBecomesKeyOnlyIfNeeded(true);
        panel.setHidesOnDeactivate(false);
        panel.setWorksWhenModal(true);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setHasShadow(true);
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setAnimationBehavior(NSWindowAnimationBehavior::None);
        panel.setAcceptsMouseMovedEvents(true);

        let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), frame);
        effect.setMaterial(NSVisualEffectMaterial::HUDWindow);
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setState(NSVisualEffectState::Active);
        effect.setMaskImage(Some(&mask_image(CORNER_RADIUS)));
        panel.setContentView(Some(&effect));

        let view = SwitcherView::new(mtm, frame, Rc::from(on_click), Rc::from(on_hover));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        effect.addSubview(&view);

        Switcher { mtm, panel, effect, view }
    }

    /// Lay out `tiles` and show the panel centered on the screen containing the mouse
    /// cursor (or the main screen). Never activates the app or takes key focus.
    /// An empty `tiles` hides the panel.
    pub fn show(&self, tiles: Vec<Tile>, selected: usize) {
        if tiles.is_empty() {
            self.ivars_mut(|st| {
                st.tiles.clear();
                st.layout = Layout::default();
                st.selected = 0;
            });
            self.hide();
            return;
        }
        let vf = target_screen(self.mtm)
            .map(|s| s.visibleFrame())
            .unwrap_or(ns_rect(0.0, 0.0, 1440.0, 900.0));
        let items: Vec<(f64, usize)> = tiles.iter().map(|t| (t.aspect, t.group)).collect();
        let layout = compute_layout(&items, vf.size.width * 0.9, vf.size.height * 0.85);
        let (w, h) = (layout.width, layout.height);
        self.ivars_mut(|st| {
            st.selected = selected.min(tiles.len() - 1);
            st.tiles = tiles;
            st.layout = layout;
            st.hovered = None;
            st.pressed = None;
        });
        let x = (vf.origin.x + (vf.size.width - w) / 2.0).round();
        let y = (vf.origin.y + (vf.size.height - h) / 2.0).round();
        self.panel.setFrame_display(ns_rect(x, y, w, h), false);
        self.view.setNeedsDisplay(true);
        self.panel.orderFrontRegardless();
        // The (layer-backed) view draws at the Core Animation commit at the end of the
        // current run-loop turn, before anything reaches the screen; the shadow is
        // recomputed from the new shape at the same time.
        self.panel.invalidateShadow();
    }

    fn ivars_mut<R>(&self, f: impl FnOnce(&mut ViewState) -> R) -> R {
        f(&mut self.view.ivars().state.borrow_mut())
    }

    /// Select tile `idx` (clamped to the last tile).
    pub fn set_selected(&self, idx: usize) {
        let st = &mut *self.view.ivars().state.borrow_mut();
        if st.tiles.is_empty() {
            return;
        }
        let idx = idx.min(st.tiles.len() - 1);
        if idx == st.selected {
            return;
        }
        let old = st.selected;
        st.selected = idx;
        self.view.invalidate_tile(st, old);
        self.view.invalidate_tile(st, idx);
    }

    pub fn selected(&self) -> usize {
        self.view.ivars().state.borrow().selected
    }

    pub fn len(&self) -> usize {
        self.view.ivars().state.borrow().tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace the thumbnail of tile `idx`; ignored when out of range.
    pub fn set_thumbnail(&self, idx: usize, image: Retained<NSImage>) {
        let st = &mut *self.view.ivars().state.borrow_mut();
        if let Some(t) = st.tiles.get_mut(idx) {
            t.thumbnail = Some(image);
            self.view.invalidate_tile(st, idx);
        }
    }

    /// Order the panel out. Tiles and selection are kept until the next `show`,
    /// so `selected()` stays valid after hiding.
    pub fn hide(&self) {
        self.panel.orderOut(None);
        self.ivars_mut(|st| {
            st.hovered = None;
            st.pressed = None;
        });
    }

    pub fn is_visible(&self) -> bool {
        self.panel.isVisible()
    }

    /// Index of the tile above the selected one in the grid (clamps on the top row).
    pub fn index_above(&self) -> usize {
        let st = self.view.ivars().state.borrow();
        st.layout.vertical_neighbor(st.selected, -1)
    }

    /// Index of the tile below the selected one in the grid (clamps on the bottom row).
    pub fn index_below(&self) -> usize {
        let st = self.view.ivars().state.borrow();
        st.layout.vertical_neighbor(st.selected, 1)
    }

    /// Render the current panel contents to a PNG at backing resolution. The live
    /// behind-window blur cannot be captured this way, so an approximate opaque HUD
    /// backdrop is painted for the snapshot only.
    pub fn snapshot_png(&self, path: &str) -> bool {
        let content: &NSView = &self.effect;
        let bounds = content.bounds();
        self.ivars_mut(|st| st.snapshot_backdrop = true);
        let rep = content.bitmapImageRepForCachingDisplayInRect(bounds);
        if let Some(rep) = &rep {
            content.cacheDisplayInRect_toBitmapImageRep(bounds, rep);
        }
        self.ivars_mut(|st| st.snapshot_backdrop = false);
        let Some(rep) = rep else { return false };
        // SAFETY: empty properties dictionary.
        let data = unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        };
        match data {
            Some(d) => d.writeToFile_atomically(&NSString::from_str(path), true),
            None => false,
        }
    }

    // ---- extras (not required by the integrator, useful for tests/integration) ----

    /// The underlying panel (e.g. for its window number).
    pub fn panel(&self) -> &NSPanel {
        &self.panel
    }

    /// Force light (`Some(false)`), dark (`Some(true)`) or system (`None`) appearance.
    pub fn set_dark(&self, dark: Option<bool>) {
        let app = dark.and_then(|d| {
            // SAFETY: AppKit-provided appearance name statics.
            let name = unsafe { if d { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
            NSAppearance::appearanceNamed(name)
        });
        self.panel.setAppearance(app.as_deref());
        self.view.setNeedsDisplay(true);
    }

    /// Current layout (flipped panel coordinates).
    pub fn layout(&self) -> Layout {
        self.view.ivars().state.borrow().layout.clone()
    }

    /// The tile-drawing view (receives mouse events).
    pub fn view(&self) -> &NSView {
        &self.view
    }
}

// ---------------------------------------------------------------------------
// Tests (pure layout / hit testing)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn items(groups: &[usize], aspect: f64) -> Vec<(f64, usize)> {
        groups.iter().enumerate().flat_map(|(g, &n)| (0..n).map(move |_| (aspect, g))).collect()
    }

    #[test]
    fn every_tile_center_hits_itself_and_gaps_miss() {
        let it = items(&[4, 3, 2, 3], 1.6);
        let l = compute_layout(&it, 1300.0, 900.0);
        assert_eq!(l.tiles.len(), 12);
        for (i, r) in l.tiles.iter().enumerate() {
            assert_eq!(l.hit_test(r.cx(), r.cy()), Some(i));
            assert_eq!(l.hit_test(r.x + 0.5, r.y + 0.5), Some(i));
            assert_eq!(l.hit_test(r.x + r.w - 0.5, r.y + r.h - 0.5), Some(i));
        }
        assert_eq!(l.hit_test(1.0, 1.0), None);
        assert_eq!(l.hit_test(-5.0, 50.0), None);
        // Gap between two tiles of the same group.
        let (a, b) = (l.tiles[0], l.tiles[1]);
        assert!(b.x > a.x + a.w);
        assert_eq!(l.hit_test(a.x + a.w + (b.x - a.x - a.w) / 2.0, a.cy()), None);
        // No overlaps and everything inside the panel.
        for (i, a) in l.tiles.iter().enumerate() {
            assert!(a.x >= PANEL_PAD && a.x + a.w <= l.width - PANEL_PAD + 0.5);
            assert!(a.y >= PANEL_PAD && a.y + a.h <= l.height - PANEL_PAD + 0.5);
            for b in &l.tiles[i + 1..] {
                assert!(!a.intersects(b));
            }
        }
    }

    #[test]
    fn groups_are_not_split_when_they_fit() {
        // Row fits ~4 tiles; groups of 3,3,2 -> [3],[3],[2] or [3],[3,2]? 3+2 doesn't fit.
        let th = BASE_THUMB_H;
        let tw = tile_w(1.6, th);
        let max_w = 2.0 * PANEL_PAD + 2.0 * GROUP_PAD + 4.0 * tw + 3.0 * TILE_GAP + 1.0;
        let it = items(&[3, 3, 2], 1.6);
        let l = compute_layout(&it, max_w, 5000.0);
        for g in 0..3 {
            let rows: std::collections::BTreeSet<usize> =
                (0..it.len()).filter(|&i| it[i].1 == g).map(|i| l.tile_row[i]).collect();
            assert_eq!(rows.len(), 1, "group {g} split across rows {rows:?}");
        }
        assert_eq!(l.groups.len(), 3);
        // Group wider than a row gets split, in order.
        let it = items(&[7], 1.6);
        let l = compute_layout(&it, max_w, 5000.0);
        assert_eq!(l.rows, vec![vec![0, 1, 2, 3], vec![4, 5, 6]]);
        assert_eq!(l.groups.len(), 2);
    }

    #[test]
    fn small_groups_share_a_row() {
        let it = items(&[1, 1, 1], 1.6);
        let l = compute_layout(&it, 2000.0, 1000.0);
        assert_eq!(l.rows.len(), 1);
        assert_eq!(l.groups.len(), 3);
        assert_eq!(l.thumb_h, BASE_THUMB_H);
    }

    #[test]
    fn forty_tiles_fit_the_height_cap() {
        let it = items(&[10, 8, 6, 5, 4, 3, 2, 1, 1], 1.6);
        assert_eq!(it.len(), 40);
        let (mw, mh) = (1512.0 * 0.9, 945.0 * 0.85);
        let l = compute_layout(&it, mw, mh);
        assert!(l.height <= mh, "height {} > {}", l.height, mh);
        assert!(l.width <= mw, "width {} > {}", l.width, mw);
        assert!(l.thumb_h < BASE_THUMB_H);
        for (i, r) in l.tiles.iter().enumerate() {
            assert_eq!(l.hit_test(r.cx(), r.cy()), Some(i));
        }
    }

    #[test]
    fn empty_and_single() {
        let l = compute_layout(&[], 1000.0, 800.0);
        assert!(l.tiles.is_empty());
        assert_eq!(l.hit_test(10.0, 10.0), None);
        assert_eq!(l.vertical_neighbor(0, 1), 0);
        let l = compute_layout(&[(1.5, 0)], 1000.0, 800.0);
        assert_eq!(l.rows, vec![vec![0]]);
        assert_eq!(l.vertical_neighbor(0, -1), 0);
        assert_eq!(l.vertical_neighbor(0, 1), 0);
        // Out-of-range index clamps instead of panicking.
        assert_eq!(l.vertical_neighbor(9, 1), 0);
    }

    #[test]
    fn up_down_navigation_picks_nearest_column_and_clamps() {
        let it = items(&[1; 9], 1.6);
        let th = BASE_THUMB_H;
        let seg = 2.0 * GROUP_PAD + tile_w(1.6, th);
        let max_w = 2.0 * PANEL_PAD + 4.0 * seg + 3.0 * GROUP_GAP + 1.0;
        let l = compute_layout(&it, max_w, 5000.0);
        assert_eq!(l.rows, vec![vec![0, 1, 2, 3], vec![4, 5, 6, 7], vec![8]]);
        assert_eq!(l.vertical_neighbor(1, 1), 5);
        assert_eq!(l.vertical_neighbor(5, -1), 1);
        assert_eq!(l.vertical_neighbor(1, -1), 1); // top row clamps
        assert_eq!(l.vertical_neighbor(8, 1), 8); // bottom row clamps
        // Last row (1 tile, centered) maps up to a middle column.
        let up = l.vertical_neighbor(8, -1);
        assert!(up == 5 || up == 6, "got {up}");
        // From the edges of row 1 down to the lone centered tile.
        assert_eq!(l.vertical_neighbor(4, 1), 8);
        assert_eq!(l.vertical_neighbor(7, 1), 8);
    }

    #[test]
    fn aspect_is_clamped_and_bad_values_survive() {
        let l = compute_layout(&[(10.0, 0), (0.1, 0), (f64::NAN, 1), (0.0, 2)], 3000.0, 1000.0);
        let th = l.thumb_h;
        assert_eq!(l.tiles[0].w, (th * MAX_ASPECT).round() + 2.0 * TILE_PAD);
        assert_eq!(l.tiles[1].w, (th * MIN_ASPECT).round() + 2.0 * TILE_PAD);
        assert!(l.tiles[2].w.is_finite() && l.tiles[3].w.is_finite());
    }
}
