//! ZTab switcher panel: a non-activating HUD panel that shows window tiles in a
//! wrapping, app-grouped grid. Everything here is main-thread only, except the tile-label
//! rendering of the layer tree (`render_label`, on the `ztab-labels` thread).
//!
//! Structure:
//! - `compute_layout` / `Layout` are pure geometry (no AppKit), unit tested below.
//! - `SwitcherView` is one flipped custom NSView that draws every tile in `drawRect:`
//!   and maps mouse events to tile indices through `Layout::hit_test`.
//! - `Switcher` owns the NSPanel (content = NSVisualEffectView, HUD material, rounded
//!   via a 9-slice mask image) and the view.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
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
use objc2_core_foundation::{CFRetained, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColor, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, CGInterpolationQuality,
};
use objc2_foundation::{
    NSAttributedStringKey, NSDictionary, NSEdgeInsets, NSObject, NSObjectProtocol, NSPoint,
    NSRect, NSSize, NSString,
};
use objc2_quartz_core::{CALayer, CATransaction};

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
    /// The app icon as a bitmap (layer tree: the big icon of a tile without a thumbnail).
    pub icon_cg: Option<CFRetained<CGImage>>,
    /// The app icon's bitmaps at their native sizes (any order): the layer tree's label row draws
    /// the one that fits its small icon, off the main thread. Empty: `icon_cg`, scaled.
    pub icon_reps: Vec<CFRetained<CGImage>>,
    pub thumbnail: Option<Retained<NSImage>>,
    /// The thumbnail's bitmap (layer tree: the thumbnail layer's contents, as is). Falls back to
    /// the NSImage's CGImage when missing.
    pub thumb_cg: Option<CFRetained<CGImage>>,
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
    /// Measurement only (panel bench): leave out the panel tint and the group backgrounds.
    skip_fills: bool,
}

pub struct ViewIvars {
    state: RefCell<ViewState>,
    /// Longest `drawRect:` (ms) since `take_max_draw_ms` (diagnostics).
    max_draw_ms: std::cell::Cell<f64>,
    on_click: Rc<dyn Fn(usize)>,
    on_hover: Rc<dyn Fn(usize)>,
    /// Layer-tree mode: the panel's content is a tree of Core Animation layers (see `sync_tree`)
    /// instead of being drawn in `drawRect:` (which stays for the PNG snapshot).
    use_layers: std::cell::Cell<bool>,
    /// The layers, created on first use.
    tree: RefCell<Option<Tree>>,
    /// Label bitmaps (layer tree), rendered off the main thread.
    labels: RefCell<LabelCache>,
}

/// An image as part of a cache key: compared and hashed by address, and kept alive by the key,
/// so no other image can get its address while a key holds it.
#[derive(Clone)]
struct ImgId(CFRetained<CGImage>);

impl ImgId {
    fn addr(&self) -> usize {
        let p: *const CGImage = &*self.0;
        p as usize
    }
}

impl PartialEq for ImgId {
    fn eq(&self, o: &Self) -> bool {
        self.addr() == o.addr()
    }
}

impl Eq for ImgId {}

impl std::hash::Hash for ImgId {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.addr().hash(h);
    }
}

impl fmt::Debug for ImgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImgId({:#x})", self.addr())
    }
}

/// What a tile's label image shows (its cache key): everything `render_label` draws from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LabelKey {
    text: String,
    /// The small icon's bitmap.
    icon: Option<ImgId>,
    width_px: u32,
    /// Pixels per point × 100.
    scale: u32,
    selected: bool,
    dark: bool,
    /// The text colour's components (bits), as resolved for the panel's appearance.
    color: Vec<u64>,
}

impl LabelKey {
    /// Whether `self` and `o` show the same title and icon (they may differ in width, weight,
    /// scale or colour): a layer showing one may keep showing it until the other is rendered.
    fn same_content(&self, o: &LabelKey) -> bool {
        self.text == o.text && self.icon == o.icon
    }
}

/// Rendered labels kept for later shows (least recently used dropped beyond this many bytes:
/// about 160 labels at 2x, every window selected and not on three displays).
const LABEL_CACHE_BYTES: usize = 12 << 20;

type Landed = Arc<Mutex<Vec<(LabelKey, Option<CFRetained<CGImage>>)>>>;

/// The label bitmaps of one view: the cache, the renders queued on the `ztab-labels` thread, and
/// the inbox their results land in.
#[derive(Default)]
struct LabelCache {
    map: HashMap<LabelKey, (CFRetained<CGImage>, usize, u64)>,
    bytes: usize,
    tick: u64,
    pending: HashSet<LabelKey>,
    inbox: Landed,
    /// Labels rendered so far (diagnostics).
    rendered: u64,
}

impl LabelCache {
    fn get(&mut self, key: &LabelKey) -> Option<CFRetained<CGImage>> {
        let tick = self.tick;
        self.map.get_mut(key).map(|e| {
            e.2 = tick;
            e.0.clone()
        })
    }

    fn insert(&mut self, key: LabelKey, img: CFRetained<CGImage>) {
        let bytes = CGImage::bytes_per_row(Some(&img)) * CGImage::height(Some(&img));
        if let Some(old) = self.map.insert(key, (img, bytes, self.tick)) {
            self.bytes -= old.1;
        }
        self.bytes += bytes;
        // Least recently used first; never what the current show uses.
        while self.bytes > LABEL_CACHE_BYTES {
            let Some(k) = self.map.iter().filter(|(_, e)| e.2 < self.tick).min_by_key(|(_, e)| e.2).map(|(k, _)| k.clone()) else {
                break;
            };
            if let Some(e) = self.map.remove(&k) {
                self.bytes -= e.1;
            }
        }
    }

    /// Queue `key` for rendering unless it is cached or queued. Returns whether it was queued.
    fn request(&mut self, key: &LabelKey, color: &Retained<CGColor>) -> bool {
        if self.map.contains_key(key) || self.pending.contains(key) {
            return false;
        }
        self.pending.insert(key.clone());
        let job = LabelJob { key: key.clone(), color: color.clone(), inbox: self.inbox.clone() };
        if label_worker().lock().map(|tx| tx.send(job).is_ok()).unwrap_or(false) {
            true
        } else {
            self.pending.remove(key);
            false
        }
    }
}

/// One label for the `ztab-labels` thread.
struct LabelJob {
    key: LabelKey,
    color: Retained<CGColor>,
    inbox: Landed,
}

// Main thread: the views whose labels land through `labels_landed`.
thread_local! {
    static LABEL_VIEWS: RefCell<Vec<objc2::rc::Weak<SwitcherView>>> = const { RefCell::new(Vec::new()) };
}

/// The `ztab-labels` thread: renders labels (AppKit string drawing into a bitmap context, which
/// AppKit supports on any thread with that thread's own graphics context) and hands them to the
/// main thread in batches (at least every 8 ms while it is busy).
fn label_worker() -> &'static Mutex<mpsc::Sender<LabelJob>> {
    static TX: OnceLock<Mutex<mpsc::Sender<LabelJob>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<LabelJob>();
        let _ = std::thread::Builder::new().name("ztab-labels".into()).spawn(move || {
            let post = || dispatch2::DispatchQueue::main().exec_async(labels_landed);
            let mut next: Option<LabelJob> = None;
            let mut last_post = std::time::Instant::now();
            loop {
                let job = match next.take() {
                    Some(j) => j,
                    None => match rx.recv() {
                        Ok(j) => j,
                        Err(_) => return,
                    },
                };
                let img = autoreleasepool(|_| render_label(&job.key, &job.color));
                if let Ok(mut v) = job.inbox.lock() {
                    v.push((job.key, img));
                }
                match rx.try_recv() {
                    Ok(j) => {
                        next = Some(j);
                        if last_post.elapsed() >= std::time::Duration::from_millis(8) {
                            post();
                            last_post = std::time::Instant::now();
                        }
                    }
                    Err(_) => {
                        post();
                        last_post = std::time::Instant::now();
                    }
                }
            }
        });
        Mutex::new(tx)
    })
}

/// Main thread: rendered labels landed; every view takes its own.
fn labels_landed() {
    let views: Vec<Retained<SwitcherView>> = LABEL_VIEWS.with(|v| {
        let mut v = v.borrow_mut();
        v.retain(|w| w.load().is_some());
        v.iter().filter_map(|w| w.load()).collect()
    });
    for view in views {
        view.take_landed_labels();
    }
}

/// The layers of one tile.
struct TileLayers {
    /// Fill shown while the tile has no thumbnail, with the big app icon on it.
    placeholder: Retained<CALayer>,
    icon: Retained<CALayer>,
    thumb: Retained<CALayer>,
    badge: Retained<CALayer>,
    /// Small icon and title, one bitmap (cached by `LabelKey`).
    label: Retained<CALayer>,
    label_key: Option<LabelKey>,
}

/// Layer-tree mode: every layer of the panel's content. Fills, borders and the selection ring
/// are layer properties and images are layer contents, so the window server composites them on
/// the GPU: the main thread rasterises nothing but the small label bitmaps (cached), there is no
/// panel-sized backing store, and a panel that moves to another display needs no redraw.
struct Tree {
    tint: Retained<CALayer>,
    groups: Vec<Retained<CALayer>>,
    selection: Retained<CALayer>,
    tiles: Vec<TileLayers>,
}

/// Colors of the layer tree, resolved for the panel's appearance.
struct Palette {
    dark: bool,
    tint: Retained<CGColor>,
    edge: Retained<CGColor>,
    group_fill: Retained<CGColor>,
    group_edge: Retained<CGColor>,
    accent_fill: Retained<CGColor>,
    accent: Retained<CGColor>,
    placeholder: Retained<CGColor>,
    thumb_edge: Retained<CGColor>,
    /// The label text colour, resolved (a fixed colour, valid on any thread).
    text: Retained<CGColor>,
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

        /// Layer-tree mode: no backing store; `drawRect:` is only used by the PNG snapshot.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            self.ivars().use_layers.get()
        }

        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {}

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            let t = std::time::Instant::now();
            self.draw(Rect::from_ns(dirty));
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let m = &self.ivars().max_draw_ms;
            m.set(m.get().max(ms));
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

/// The thumbnail area of tile rect `r` (thumbnail height `th`).
fn thumb_area(r: Rect, th: f64) -> Rect {
    Rect::new(r.x + TILE_PAD, r.y + TILE_PAD, r.w - 2.0 * TILE_PAD, th)
}

/// Where a thumbnail image goes inside its tile's thumbnail `area`.
fn thumb_rect(area: Rect, img: &NSImage, aspect: f64) -> Rect {
    let sz = img.size();
    area.fit(if sz.height > 0.0 { sz.width / sz.height } else { aspect })
}

/// The CGImage behind a thumbnail NSImage (the thumbnailer builds them from plain bitmaps, so
/// this hands back that bitmap without drawing).
fn cg_image_of(img: &NSImage) -> Option<Retained<CGImage>> {
    // SAFETY: a null proposed rect and no context or hints are allowed.
    unsafe { img.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }
}

/// The "Minimized" badge (pill and text) as a 2x bitmap, and its size in points. Built once.
fn badge_image() -> Option<(CFRetained<CGImage>, f64, f64)> {
    thread_local! {
        static BADGE: RefCell<Option<Option<(CFRetained<CGImage>, f64, f64)>>> = const { RefCell::new(None) };
    }
    BADGE.with(|b| {
        b.borrow_mut()
            .get_or_insert_with(|| {
                let font = NSFont::systemFontOfSize_weight(10.0, unsafe { NSFontWeightSemibold });
                let attrs = text_attrs(&font, &NSColor::whiteColor(), false);
                let s = NSString::from_str("Minimized");
                // SAFETY: attrs is a valid attribute dictionary.
                let sz = unsafe { s.sizeWithAttributes(Some(&attrs)) };
                let (w, h) = ((sz.width + 14.0).round(), (sz.height + 4.0).round());
                let scale = 2.0;
                let (pw, ph) = ((w * scale) as usize, (h * scale) as usize);
                // SAFETY: CoreGraphics-provided constant.
                let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))?;
                let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
                // SAFETY: null data lets CG allocate; sizes are positive.
                let ctx: CFRetained<CGContext> =
                    unsafe { CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, 0, Some(&cs), info)? };
                CGContext::scale_ctm(Some(&ctx), scale, scale);
                let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, false);
                NSGraphicsContext::saveGraphicsState_class();
                NSGraphicsContext::setCurrentContext(Some(&gc));
                NSColor::colorWithWhite_alpha(0.0, 0.62).setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(ns_rect(0.0, 0.0, w, h), h / 2.0, h / 2.0).fill();
                // SAFETY: attrs is a valid attribute dictionary.
                unsafe { s.drawInRect_withAttributes(ns_rect(7.0, (h - sz.height) / 2.0, sz.width + 1.0, sz.height), Some(&attrs)) };
                NSGraphicsContext::restoreGraphicsState_class();
                Some((CGBitmapContextCreateImage(Some(&ctx))?, w, h))
            })
            .clone()
    })
}

/// A tile's label row (small app icon, then the title, truncated) as a bitmap: `key.width_px`
/// pixels wide at `key.scale` pixels per point, in sRGB, the text in `color`. Any thread (the
/// `ztab-labels` thread): AppKit string drawing in this thread's own graphics context, the icon
/// drawn from its bitmap, no dynamic colour.
fn render_label(key: &LabelKey, color: &CGColor) -> Option<CFRetained<CGImage>> {
    let scale = f64::from(key.scale.max(1)) / 100.0;
    let w = f64::from(key.width_px.max(1)) / scale;
    let (pw, ph) = (key.width_px.max(1) as usize, (LABEL_H * scale).round() as usize);
    // SAFETY: CoreGraphics-provided constant.
    let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }))?;
    let info = CGImageByteOrderInfo::Order32Little.0 | CGImageAlphaInfo::PremultipliedFirst.0;
    // SAFETY: null data lets CG allocate; sizes are positive.
    let ctx: CFRetained<CGContext> = unsafe { CGBitmapContextCreate(std::ptr::null_mut(), pw, ph, 8, 0, Some(&cs), info)? };
    // Flipped point coordinates, like the view.
    CGContext::translate_ctm(Some(&ctx), 0.0, ph as f64);
    CGContext::scale_ctm(Some(&ctx), scale, -scale);
    let mut tx = 0.0;
    if let Some(icon) = &key.icon {
        let y = ((LABEL_H - SMALL_ICON) / 2.0).round();
        CGContext::save_g_state(Some(&ctx));
        // Unflip for the image (CoreGraphics draws images bottom-up).
        CGContext::translate_ctm(Some(&ctx), 0.0, y + SMALL_ICON);
        CGContext::scale_ctm(Some(&ctx), 1.0, -1.0);
        CGContext::set_interpolation_quality(Some(&ctx), CGInterpolationQuality::High);
        CGContext::draw_image(Some(&ctx), CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(SMALL_ICON, SMALL_ICON)), Some(&icon.0));
        CGContext::restore_g_state(Some(&ctx));
        tx = SMALL_ICON + 5.0;
    }
    let gc = NSGraphicsContext::graphicsContextWithCGContext_flipped(&ctx, true);
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&gc));
    // SAFETY: AppKit-provided font weight constants.
    let font = NSFont::systemFontOfSize_weight(12.0, unsafe { if key.selected { NSFontWeightMedium } else { NSFontWeightRegular } });
    let line_h = (font.ascender() - font.descender() + font.leading()).ceil();
    let ns_color = NSColor::colorWithCGColor(color).unwrap_or_else(NSColor::blackColor);
    let attrs = text_attrs(&font, &ns_color, true);
    draw_text(&NSString::from_str(&key.text), Rect::new(tx, ((LABEL_H - line_h) / 2.0).round(), w - tx, line_h), &attrs);
    NSGraphicsContext::restoreGraphicsState_class();
    CGBitmapContextCreateImage(Some(&ctx))
}

/// Diagnostics: write `img` to `path` as a PNG.
pub fn write_png(img: &CGImage, path: &str) -> bool {
    let rep = objc2_app_kit::NSBitmapImageRep::initWithCGImage(objc2_app_kit::NSBitmapImageRep::alloc(), img);
    // SAFETY: empty properties dictionary.
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) };
    data.is_some_and(|d| d.writeToFile_atomically(&NSString::from_str(path), true))
}

/// The bitmap of `tile`'s icon for the label row's small icon at `scale`: the native one of that
/// size, else the smallest larger one, else the largest.
fn label_icon(tile: &Tile, scale: f64) -> Option<CFRetained<CGImage>> {
    let px = (SMALL_ICON * scale).round() as usize;
    let w = |i: &CFRetained<CGImage>| CGImage::width(Some(i));
    let reps: Vec<&CFRetained<CGImage>> = tile.icon_reps.iter().chain(tile.icon_cg.iter()).collect();
    reps.iter()
        .filter(|i| w(i) >= px)
        .min_by_key(|i| w(i))
        .or_else(|| reps.iter().max_by_key(|i| w(i)))
        .map(|i| (*i).clone())
}

/// The components of `c` as key bits.
fn color_bits(c: &CGColor) -> Vec<u64> {
    let n = CGColor::number_of_components(Some(c));
    let p = CGColor::components(Some(c));
    if p.is_null() {
        return Vec::new();
    }
    // SAFETY: CoreGraphics returns `n` components at `p`, valid while `c` lives.
    unsafe { std::slice::from_raw_parts(p, n) }.iter().map(|v| v.to_bits()).collect()
}

/// The label of `tile` in a tile `w` points wide, at `scale`.
fn label_key(tile: &Tile, w: f64, scale: f64, selected: bool, pal: &Palette) -> LabelKey {
    LabelKey {
        text: if tile.title.trim().is_empty() { tile.app_name.clone() } else { tile.title.clone() },
        icon: label_icon(tile, scale).map(ImgId),
        width_px: (w * scale).round().max(1.0) as u32,
        scale: (scale * 100.0).round() as u32,
        selected,
        dark: pal.dark,
        color: color_bits(&pal.text),
    }
}

impl SwitcherView {
    /// The layer tree's colors for this view's appearance.
    fn palette(&self) -> Palette {
        let out: RefCell<Option<Palette>> = RefCell::new(None);
        {
            let block = block2::RcBlock::new(|| {
                let cg = |c: Retained<NSColor>| c.CGColor();
                let label = NSColor::labelColor();
                let accent = NSColor::controlAccentColor();
                // SAFETY: AppKit-provided appearance name statics.
                let dark = unsafe {
                    NSAppearance::currentDrawingAppearance()
                        .bestMatchFromAppearancesWithNames(&objc2_foundation::NSArray::from_slice(&[
                            NSAppearanceNameAqua,
                            NSAppearanceNameDarkAqua,
                        ]))
                        .is_some_and(|n| &*n == NSAppearanceNameDarkAqua)
                };
                *out.borrow_mut() = Some(Palette {
                    dark,
                    tint: cg(NSColor::windowBackgroundColor().colorWithAlphaComponent(0.5)),
                    edge: cg(label.colorWithAlphaComponent(0.12)),
                    group_fill: cg(label.colorWithAlphaComponent(0.055)),
                    group_edge: cg(label.colorWithAlphaComponent(0.07)),
                    accent_fill: cg(accent.colorWithAlphaComponent(0.22)),
                    accent: cg(accent.clone()),
                    placeholder: cg(label.colorWithAlphaComponent(0.06)),
                    thumb_edge: cg(label.colorWithAlphaComponent(0.18)),
                    // Resolved here, in the view's appearance: a dynamic colour drawn later, outside
                    // this block, would take whatever appearance was current at launch.
                    text: cg(label),
                });
            });
            self.effectiveAppearance().performAsCurrentDrawingAppearance(&block);
        }
        out.into_inner().expect("appearance block ran")
    }

    /// Layer-tree mode: bring the layers up to date with the view state. `only`: just that tile
    /// (a thumbnail or icon arrived); `sel`: just the selection (the two tiles whose label
    /// weight changes); neither: everything. No implicit animations. Cheap: layer properties,
    /// and label bitmaps only when a label changed (cached).
    fn sync_tree(&self, only: Option<usize>, sel: Option<(usize, usize)>) {
        if !self.ivars().use_layers.get() {
            return;
        }
        let Some(root) = self.layer() else { return };
        let st = self.ivars().state.borrow();
        let mut tree_ref = self.ivars().tree.borrow_mut();
        let tree = tree_ref.get_or_insert_with(|| {
            let mk = |z: f64| {
                let l = CALayer::new();
                l.setZPosition(z);
                root.addSublayer(&l);
                l
            };
            let tint = mk(-1.0);
            tint.setCornerRadius(CORNER_RADIUS);
            tint.setBorderWidth(1.0);
            let selection = mk(1.0);
            selection.setCornerRadius(10.0);
            selection.setBorderWidth(3.0);
            Tree { tint, groups: Vec::new(), selection, tiles: Vec::new() }
        });
        let pal = self.palette();
        let bounds = self.bounds();
        // Sublayer geometry follows the backing layer's orientation (AppKit flips it for this
        // flipped view); contents images keep their orientation either way.
        let flipped = root.isGeometryFlipped();
        let height = bounds.size.height;
        let frame = |r: Rect| {
            let y = if flipped { r.y } else { height - r.y - r.h };
            CGRect::new(CGPoint::new(r.x, y), CGSize::new(r.w, r.h))
        };
        let scale = self.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0);
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        let all = only.is_none() && sel.is_none();
        if all {
            tree.tint.setFrame(frame(Rect::from_ns(bounds)));
            tree.tint.setBackgroundColor(Some(&pal.tint));
            tree.tint.setBorderColor(Some(&pal.edge));
            tree.tint.setHidden(st.skip_fills);
            for (k, (g, _)) in st.layout.groups.iter().enumerate() {
                if tree.groups.len() <= k {
                    let l = CALayer::new();
                    l.setCornerRadius(12.0);
                    l.setBorderWidth(1.0);
                    root.addSublayer(&l);
                    tree.groups.push(l);
                }
                let l = &tree.groups[k];
                l.setFrame(frame(*g));
                l.setBackgroundColor(Some(&pal.group_fill));
                l.setBorderColor(Some(&pal.group_edge));
                l.setHidden(st.skip_fills);
            }
            for l in tree.groups.iter().skip(st.layout.groups.len()) {
                l.setHidden(true);
            }
        }
        if all || sel.is_some() {
            match st.layout.tiles.get(st.selected) {
                Some(r) => {
                    tree.selection.setFrame(frame(*r));
                    tree.selection.setBackgroundColor(Some(&pal.accent_fill));
                    tree.selection.setBorderColor(Some(&pal.accent));
                    tree.selection.setHidden(false);
                }
                None => tree.selection.setHidden(true),
            }
        }
        let tiles: Vec<usize> = match (only, sel) {
            (Some(i), _) => vec![i],
            (None, Some((a, b))) => vec![a, b],
            (None, None) => (0..st.tiles.len().max(tree.tiles.len())).collect(),
        };
        let mut labels = self.ivars().labels.borrow_mut();
        labels.tick += 1;
        for i in tiles {
            let (Some(tile), Some(&r)) = (st.tiles.get(i), st.layout.tiles.get(i)) else {
                if let Some(t) = tree.tiles.get_mut(i) {
                    // A tile past this session's last: hidden, and holding no image (it would keep
                    // a closed window's thumbnail alive here and in the window server).
                    for l in [&t.placeholder, &t.icon, &t.thumb, &t.badge, &t.label] {
                        l.setHidden(true);
                    }
                    for l in [&t.icon, &t.thumb, &t.label] {
                        // SAFETY: no contents is valid.
                        unsafe { l.setContents(None) };
                    }
                    t.label_key = None;
                }
                continue;
            };
            while tree.tiles.len() <= i {
                let mk = |z: f64| {
                    let l = CALayer::new();
                    l.setZPosition(z);
                    l.setHidden(true);
                    root.addSublayer(&l);
                    l
                };
                let placeholder = mk(2.0);
                placeholder.setCornerRadius(6.0);
                let thumb = mk(3.0);
                thumb.setCornerRadius(6.0);
                thumb.setMasksToBounds(true);
                thumb.setBorderWidth(0.5);
                let tl = TileLayers { placeholder, icon: mk(3.0), thumb, badge: mk(4.0), label: mk(3.0), label_key: None };
                if let Some((img, _, _)) = badge_image() {
                    let t: &CFType = &img;
                    // SAFETY: a CGImage is valid layer contents.
                    unsafe { tl.badge.setContents(Some(t.as_ref())) };
                }
                tree.tiles.push(tl);
            }
            let tl = &mut tree.tiles[i];
            let area = thumb_area(r, st.layout.thumb_h);
            let thumb_cg = tile.thumb_cg.clone().or_else(|| tile.thumbnail.as_deref().and_then(|t| cg_image_of(t).map(|c| c.into())));
            let content = match (&tile.thumbnail, &thumb_cg) {
                (Some(img), Some(cg)) => {
                    let fr = thumb_rect(area, img, tile.aspect);
                    tl.thumb.setFrame(frame(fr));
                    let t: &CFType = cg;
                    // SAFETY: a CGImage is valid layer contents.
                    unsafe { tl.thumb.setContents(Some(t.as_ref())) };
                    tl.thumb.setBorderColor(Some(&pal.thumb_edge));
                    tl.thumb.setOpacity(if tile.minimized { 0.45 } else { 1.0 });
                    tl.thumb.setHidden(false);
                    tl.placeholder.setHidden(true);
                    tl.icon.setHidden(true);
                    fr
                }
                _ => {
                    let a = tile.aspect.clamp(MIN_ASPECT, MAX_ASPECT);
                    let fr = area.fit(if a.is_finite() { a } else { 1.6 });
                    tl.thumb.setHidden(true);
                    // SAFETY: no contents is valid (the layer may hold another window's image).
                    unsafe { tl.thumb.setContents(None) };
                    tl.placeholder.setFrame(frame(fr));
                    tl.placeholder.setBackgroundColor(Some(&pal.placeholder));
                    tl.placeholder.setHidden(false);
                    match &tile.icon_cg {
                        Some(icon) => {
                            let sz = (fr.w.min(fr.h) * 0.56).min(96.0).round();
                            let ir = Rect::new((fr.cx() - sz / 2.0).round(), (fr.cy() - sz / 2.0).round(), sz, sz);
                            tl.icon.setFrame(frame(ir));
                            let t: &CFType = icon;
                            // SAFETY: a CGImage is valid layer contents.
                            unsafe { tl.icon.setContents(Some(t.as_ref())) };
                            tl.icon.setOpacity(if tile.minimized { 0.55 } else { 1.0 });
                            tl.icon.setHidden(false);
                        }
                        None => {
                            tl.icon.setHidden(true);
                            // SAFETY: no contents is valid.
                            unsafe { tl.icon.setContents(None) };
                        }
                    }
                    fr
                }
            };
            // The badge is centered on the thumbnail (or placeholder), 6 pt above its bottom.
            match badge_image() {
                Some((_, bw, bh)) if tile.minimized && bw <= content.w - 8.0 => {
                    let br = Rect::new((content.cx() - bw / 2.0).round(), content.y + content.h - bh - 6.0, bw, bh);
                    tl.badge.setFrame(frame(br));
                    tl.badge.setHidden(false);
                }
                _ => tl.badge.setHidden(true),
            }
            // Label: small icon + title, one bitmap, rendered on the `ztab-labels` thread (queued
            // at session open by `prefetch_labels`, so it is normally cached by now). Until a
            // missing one lands, the layer keeps the same title's other rendering (other width or
            // weight) or shows nothing (`take_landed_labels` fills it in).
            let lr = Rect::new(r.x + TILE_PAD, r.y + TILE_PAD + st.layout.thumb_h + LABEL_GAP, r.w - 2.0 * TILE_PAD, LABEL_H);
            let key = label_key(tile, lr.w, scale, i == st.selected, &pal);
            if tl.label_key.as_ref() != Some(&key) {
                match labels.get(&key) {
                    Some(img) => {
                        let t: &CFType = &img;
                        // SAFETY: a CGImage is valid layer contents.
                        unsafe { tl.label.setContents(Some(t.as_ref())) };
                    }
                    None => {
                        labels.request(&key, &pal.text);
                        if !tl.label_key.as_ref().is_some_and(|k| k.same_content(&key)) {
                            // SAFETY: no contents is valid.
                            unsafe { tl.label.setContents(None) };
                        }
                    }
                }
                tl.label_key = Some(key);
            } else {
                // Keep it in the cache while it is in use.
                labels.get(&key);
            }
            tl.label.setFrame(frame(lr));
            tl.label.setHidden(false);
        }
        drop(labels);
        if all {
            // Spare tile layers beyond a few are released (they hold no images, see above).
            let keep = st.tiles.len().max(16);
            for t in tree.tiles.drain(keep.min(tree.tiles.len())..) {
                for l in [&t.placeholder, &t.icon, &t.thumb, &t.badge, &t.label] {
                    l.removeFromSuperlayer();
                }
            }
        }
        CATransaction::commit();
    }

    /// Main thread: put the labels the `ztab-labels` thread rendered into the cache and into the
    /// tile layers that wait for them.
    fn take_landed_labels(&self) {
        let landed: Vec<(LabelKey, Option<CFRetained<CGImage>>)> = {
            let labels = self.ivars().labels.borrow();
            let mut v = labels.inbox.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *v)
        };
        if landed.is_empty() {
            return;
        }
        let mut labels = self.ivars().labels.borrow_mut();
        let tree = self.ivars().tree.borrow();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        for (key, img) in landed {
            labels.pending.remove(&key);
            let Some(img) = img else { continue };
            labels.rendered += 1;
            if let Some(tree) = tree.as_ref() {
                for tl in tree.tiles.iter().filter(|tl| tl.label_key.as_ref() == Some(&key)) {
                    let t: &CFType = &img;
                    // SAFETY: a CGImage is valid layer contents.
                    unsafe { tl.label.setContents(Some(t.as_ref())) };
                }
            }
            labels.insert(key, img);
        }
        CATransaction::commit();
    }

    /// Queue the labels of `tiles` laid out on a panel `max_w` x `max_h` at `scale`, selected and
    /// not. Returns how many were queued (the others are cached or already queued).
    fn prefetch_labels(&self, tiles: &[Tile], max_w: f64, max_h: f64, scale: f64) -> usize {
        if !self.ivars().use_layers.get() || tiles.is_empty() {
            return 0;
        }
        let items: Vec<(f64, usize)> = tiles.iter().map(|t| (t.aspect, t.group)).collect();
        let layout = compute_layout(&items, max_w, max_h);
        let pal = self.palette();
        let mut labels = self.ivars().labels.borrow_mut();
        labels.tick += 1;
        let mut n = 0;
        for (tile, r) in tiles.iter().zip(layout.tiles.iter()) {
            for selected in [false, true] {
                let key = label_key(tile, r.w - 2.0 * TILE_PAD, scale, selected, &pal);
                if labels.get(&key).is_none() && labels.request(&key, &pal.text) {
                    n += 1;
                }
            }
        }
        n
    }

    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        on_click: Rc<dyn Fn(usize)>,
        on_hover: Rc<dyn Fn(usize)>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            state: RefCell::new(ViewState::default()),
            max_draw_ms: std::cell::Cell::new(0.0),
            on_click,
            on_hover,
            use_layers: std::cell::Cell::new(false),
            tree: RefCell::new(None),
            labels: RefCell::new(LabelCache::default()),
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
        LABEL_VIEWS.with(|v| v.borrow_mut().push(objc2::rc::Weak::from_retained(&view)));
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
        if !st.skip_fills {
            rounded(bounds, CORNER_RADIUS).fill();
        }
        // Hairline edge, like system HUDs.
        let edge = rounded(bounds.inset(0.5), CORNER_RADIUS - 0.5);
        edge.setLineWidth(1.0);
        NSColor::labelColor().colorWithAlphaComponent(0.12).setStroke();
        edge.stroke();

        // Group backgrounds.
        let group_fill = NSColor::labelColor().colorWithAlphaComponent(0.055);
        let group_edge = NSColor::labelColor().colorWithAlphaComponent(0.07);
        for (g, _) in &st.layout.groups {
            if !g.intersects(&dirty) || st.skip_fills {
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
        // The snapshot (cacheDisplay) cannot capture sublayers: it draws the thumbnails.
        let layers = self.ivars().use_layers.get() && !st.snapshot_backdrop;

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

            let area = thumb_area(r, th);
            let alpha = if tile.minimized { 0.45 } else { 1.0 };
            let content = match &tile.thumbnail {
                // A thumbnail layer shows it (with its badge): nothing to draw here.
                Some(img) if layers => thumb_rect(area, img, tile.aspect),
                Some(img) => {
                    let fr = thumb_rect(area, img, tile.aspect);
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

            if tile.minimized && !(layers && tile.thumbnail.is_some()) {
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

/// The panel's window level: one above pop-up menus (101), the overlay level (102).
pub const PANEL_LEVEL: isize = NSPopUpMenuWindowLevel + 1;

/// The switcher panel. Created once and reused. Main thread only (not Send/Sync).
pub struct Switcher {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    effect: Retained<NSVisualEffectView>,
    view: Retained<SwitcherView>,
    /// The panel is ordered in, fully transparent, by `warm_up`.
    warming: std::cell::Cell<bool>,
    /// Keep the panel ordered in between sessions, fully transparent and ignoring the mouse,
    /// instead of ordering it out (see `set_keep_ordered_in`).
    keep_in: std::cell::Cell<bool>,
    /// The panel is shown to the user (keep-ordered-in mode, where it is always ordered in).
    shown: std::cell::Cell<bool>,
    /// Main-thread time of the last show by step (`last_show_breakdown`).
    breakdown: RefCell<String>,
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
        // One above pop-up menus: the panel stays ordered in between sessions (keep mode), so a
        // menu or another app's pop-up-level window ordered in later would otherwise cover it.
        panel.setLevel(PANEL_LEVEL);
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
        view.setWantsLayer(true);
        // Measurement knobs (panel bench): thumbnails as layers, panel kept ordered in.
        let knob = |k: &str| std::env::var_os(k).is_some_and(|v| v == "1");
        if knob("ZTAB_PANEL_TREE") {
            view.ivars().use_layers.set(true);
        }

        let sw = Switcher {
            mtm,
            panel,
            effect,
            view,
            warming: std::cell::Cell::new(false),
            keep_in: std::cell::Cell::new(false),
            shown: std::cell::Cell::new(false),
            breakdown: RefCell::new(String::new()),
        };
        if knob("ZTAB_PANEL_KEEP") {
            sw.set_keep_ordered_in(true);
        }
        sw
    }

    /// Keep the panel ordered in between sessions (transparent, ignoring the mouse), so showing
    /// it only sets its frame and opacity: no window ordering (which waits for the window
    /// server) when ⌥Tab opens it.
    pub fn set_keep_ordered_in(&self, on: bool) {
        self.keep_in.set(on);
    }

    /// Diagnostics: how the tile view is drawn.
    pub fn layer_info(&self) -> String {
        let l = self.view.layer();
        format!(
            "layer_tree={} keep_ordered_in={} draws_async={:?} geometry_flipped={:?} contents_format={:?} window_depth={:#x} color_space={:?}",
            self.view.ivars().use_layers.get(),
            self.keep_in.get(),
            l.as_ref().map(|l| l.drawsAsynchronously()),
            l.as_ref().map(|l| l.isGeometryFlipped()),
            l.as_ref().map(|l| l.contentsFormat().to_string()),
            self.panel.depthLimit().0,
            self.panel.colorSpace().and_then(|c| c.localizedName()).map(|n| n.to_string())
        )
    }

    /// Make the panel an 8-bit sRGB window instead of taking the depth and colour space of the
    /// display it is on. Its layers' images are sRGB bitmaps (`thumbs::srgb`), so Core Animation's
    /// commit passes them on unconverted whatever the display (otherwise every image is colour
    /// converted on the main thread, again after each move to another display); the window server
    /// matches the window to the display on the GPU.
    pub fn set_backing_8bit(&self, on: bool) {
        if !on {
            return;
        }
        self.panel.setDynamicDepthLimit(false);
        self.panel.setDepthLimit(objc2_app_kit::NSWindowDepth::TwentyfourBitRGB);
        self.panel.setColorSpace(Some(&objc2_app_kit::NSColorSpace::sRGBColorSpace()));
        if let Some(l) = self.view.layer() {
            // SAFETY: AppKit-provided constant.
            l.setContentsFormat(unsafe { objc2_quartz_core::kCAContentsFormatRGBA8Uint });
        }
    }

    /// Measurement only: leave out the panel tint and group backgrounds.
    pub fn set_skip_fills(&self, on: bool) {
        self.ivars_mut(|st| st.skip_fills = on);
    }

    /// Layer-tree mode (see `SwitcherView::sync_tree`) or `drawRect:` drawing. Set it before
    /// the first show.
    pub fn set_layer_tree(&self, on: bool) {
        self.view.ivars().use_layers.set(on);
        if !on && let Some(t) = self.view.ivars().tree.borrow_mut().take() {
            t.tint.removeFromSuperlayer();
            t.selection.removeFromSuperlayer();
            for l in &t.groups {
                l.removeFromSuperlayer();
            }
            for tl in &t.tiles {
                for l in [&tl.placeholder, &tl.icon, &tl.thumb, &tl.badge, &tl.label] {
                    l.removeFromSuperlayer();
                }
            }
        }
        self.view.setNeedsDisplay(true);
    }

    /// Main-thread time of the last show, by step (ms), for the log.
    pub fn last_show_breakdown(&self) -> String {
        self.breakdown.borrow().clone()
    }

    /// Lay out `tiles` and show the panel centered on the screen containing the mouse
    /// cursor (or the main screen). Never activates the app or takes key focus.
    /// An empty `tiles` hides the panel.
    pub fn show(&self, tiles: Vec<Tile>, selected: usize) {
        self.show_on(tiles, selected, None);
    }

    /// Like [`show`](Self::show), on `screen` (`None`: the screen with the mouse cursor).
    pub fn show_on(&self, tiles: Vec<Tile>, selected: usize, screen: Option<&NSScreen>) {
        self.place(tiles, selected, screen, false);
    }

    /// Lay out, position and order in the panel; `warm`: keep it transparent (warm-up).
    fn place(&self, tiles: Vec<Tile>, selected: usize, screen: Option<&NSScreen>, warm: bool) {
        if tiles.is_empty() {
            self.ivars_mut(|st| {
                st.tiles.clear();
                st.layout = Layout::default();
                st.selected = 0;
            });
            self.view.sync_tree(None, None);
            self.hide();
            return;
        }
        let t0 = std::time::Instant::now();
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        let vf = screen
            .map(|s| s.visibleFrame())
            .or_else(|| target_screen(self.mtm).map(|s| s.visibleFrame()))
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
        let t_layout = ms(t0);
        // Kept ordered in (transparent): no ordering below, only the frame and the opacity.
        let ordered_in = self.keep_in.get() && self.panel.isVisible();
        let was_warming = self.warming.replace(warm);
        if warm {
            self.panel.setAlphaValue(0.0);
            self.panel.setIgnoresMouseEvents(true);
        } else if !ordered_in && (was_warming || self.keep_in.get()) {
            // Ordered in below: opaque at once. In keep mode the panel is transparent whenever it
            // is not shown; something else (the system, a display change) may have ordered it out.
            self.panel.setAlphaValue(1.0);
        }
        let t = std::time::Instant::now();
        self.panel.setFrame_display(ns_rect(x, y, w, h), false);
        let t_frame = ms(t);
        let t = std::time::Instant::now();
        if self.view.ivars().use_layers.get() {
            self.view.sync_tree(None, None);
        } else {
            self.view.setNeedsDisplay(true);
        }
        let t_content = ms(t);
        let t = std::time::Instant::now();
        if !ordered_in {
            self.panel.orderFrontRegardless();
        }
        let t_order = ms(t);
        self.shown.set(true);
        // The shadow is recomputed from the new shape at the commit.
        self.panel.invalidateShadow();
        // Commit now (the content, and in drawRect mode its first draw) rather than at the end
        // of the run-loop turn, so the panel is on its way to the screen when this returns and
        // the time is measured here.
        let t = std::time::Instant::now();
        CATransaction::flush();
        let t_commit = ms(t);
        if !warm && ordered_in {
            // After the commit, so the previous session's content cannot flash: the window
            // server gets the new layers before the panel becomes opaque.
            self.panel.setAlphaValue(1.0);
            self.panel.setIgnoresMouseEvents(false);
        } else if !warm && (was_warming || self.keep_in.get()) {
            self.panel.setIgnoresMouseEvents(false);
        }
        *self.breakdown.borrow_mut() = format!(
            "layout={t_layout:.1} frame={t_frame:.1} content={t_content:.1} order={t_order:.1} commit={t_commit:.1} total={:.1}",
            ms(t0)
        );
    }

    fn ivars_mut<R>(&self, f: impl FnOnce(&mut ViewState) -> R) -> R {
        f(&mut self.view.ivars().state.borrow_mut())
    }

    /// Select tile `idx` (clamped to the last tile).
    pub fn set_selected(&self, idx: usize) {
        let changed = {
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
            if !self.view.ivars().use_layers.get() {
                self.view.invalidate_tile(st, old);
                self.view.invalidate_tile(st, idx);
            }
            (old, idx)
        };
        self.view.sync_tree(None, Some(changed));
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

    /// Replace the thumbnail of tile `idx` (`cg`: its bitmap); ignored when out of range.
    pub fn set_thumbnail(&self, idx: usize, image: Retained<NSImage>, cg: Option<CFRetained<CGImage>>) {
        {
            let st = &mut *self.view.ivars().state.borrow_mut();
            let Some(t) = st.tiles.get_mut(idx) else { return };
            t.thumbnail = Some(image);
            t.thumb_cg = cg;
            if !self.view.ivars().use_layers.get() {
                self.view.invalidate_tile(st, idx);
            }
        }
        self.view.sync_tree(Some(idx), None);
    }

    /// Replace the app icon of tile `idx` (e.g. once it finished loading off the main thread);
    /// ignored when out of range.
    pub fn set_icon(&self, idx: usize, icon: Retained<NSImage>, icon_cg: Option<CFRetained<CGImage>>, icon_reps: Vec<CFRetained<CGImage>>) {
        {
            let st = &mut *self.view.ivars().state.borrow_mut();
            let Some(t) = st.tiles.get_mut(idx) else { return };
            t.icon = Some(icon);
            t.icon_cg = icon_cg;
            t.icon_reps = icon_reps;
            if !self.view.ivars().use_layers.get() {
                self.view.invalidate_tile(st, idx);
            }
        }
        self.view.sync_tree(Some(idx), None);
    }

    /// Whether tile `idx` currently has a thumbnail image.
    pub fn has_thumbnail(&self, idx: usize) -> bool {
        self.view.ivars().state.borrow().tiles.get(idx).is_some_and(|t| t.thumbnail.is_some())
    }

    /// Session open: queue the tile labels for the screen the panel will show on (the mouse's),
    /// selected and not, so the `ztab-labels` thread renders them during the show delay. Cheap
    /// (a layout and a few hash lookups). Returns how many were queued.
    pub fn prefetch_labels(&self, tiles: &[Tile]) -> usize {
        let Some(screen) = target_screen(self.mtm) else { return 0 };
        let vf = screen.visibleFrame();
        self.view.prefetch_labels(tiles, vf.size.width * 0.9, vf.size.height * 0.85, screen.backingScaleFactor())
    }

    /// Diagnostics: the label bitmap tile `idx` shows (once rendered), and whether the panel's
    /// palette is dark.
    pub fn label_image(&self, idx: usize) -> (Option<CFRetained<CGImage>>, bool) {
        let dark = self.view.palette().dark;
        let tree = self.view.ivars().tree.borrow();
        let key = tree.as_ref().and_then(|t| t.tiles.get(idx)).and_then(|tl| tl.label_key.clone());
        let img = key.and_then(|k| self.view.ivars().labels.borrow().map.get(&k).map(|e| e.0.clone()));
        (img, dark)
    }

    /// Layer tree: tile layer sets, and how many of them hold a thumbnail, icon or label image.
    pub fn tree_stats(&self) -> (usize, usize) {
        let tree = self.view.ivars().tree.borrow();
        let Some(t) = tree.as_ref() else { return (0, 0) };
        // SAFETY: reading the contents property of our own layers.
        let held = t.tiles.iter().filter(|tl| [&tl.thumb, &tl.icon, &tl.label].iter().any(|l| unsafe { l.contents() }.is_some())).count();
        (t.tiles.len(), held)
    }

    /// Labels rendered off the main thread so far, labels cached (count, KB), renders queued.
    pub fn label_stats(&self) -> (u64, usize, usize, usize) {
        let l = self.view.ivars().labels.borrow();
        (l.rendered, l.map.len(), l.bytes / 1024, l.pending.len())
    }

    /// Keep mode, between sessions: order the transparent panel to the front of its level again,
    /// so a window of the same level ordered in since does not cover the next show. Off the show
    /// path (call it after `hide`). Returns the time it took (ms), or `None` when not applicable.
    pub fn refront_if_kept(&self) -> Option<f64> {
        if !self.keep_in.get() || self.shown.get() || self.warming.get() || !self.panel.isVisible() {
            return None;
        }
        let t = std::time::Instant::now();
        self.panel.orderFrontRegardless();
        Some(t.elapsed().as_secs_f64() * 1000.0)
    }

    /// Order the panel out. Tiles and selection are kept until the next `show`,
    /// so `selected()` stays valid after hiding.
    pub fn hide(&self) {
        if self.keep_in.get() {
            self.panel.setAlphaValue(0.0);
            self.panel.setIgnoresMouseEvents(true);
        } else {
            self.panel.orderOut(None);
        }
        self.shown.set(false);
        self.ivars_mut(|st| {
            st.hovered = None;
            st.pressed = None;
        });
    }

    /// Longest `drawRect:` of the panel (ms) since the last call (diagnostics).
    pub fn take_max_draw_ms(&self) -> f64 {
        self.view.ivars().max_draw_ms.replace(0.0)
    }

    pub fn is_visible(&self) -> bool {
        self.shown.get() && self.panel.isVisible() && !self.warming.get()
    }

    /// Show `tiles` once, fully transparent and ignoring the mouse, so the one-time costs of the
    /// first real show (window surface, visual-effect material, first layout and draw, fonts)
    /// are paid now: call it at launch, then `end_warm_up` a run-loop turn or more later (after
    /// Core Animation committed the frame). A real `show` in between simply takes over.
    pub fn warm_up(&self, tiles: Vec<Tile>) {
        self.warm_up_on(tiles, None);
    }

    /// [`warm_up`](Self::warm_up) on `screen` (`None`: the screen with the mouse cursor).
    pub fn warm_up_on(&self, tiles: Vec<Tile>, screen: Option<&NSScreen>) {
        if tiles.is_empty() || self.is_visible() {
            return;
        }
        self.place(tiles, 0, screen, true);
        self.panel.display();
    }

    pub fn end_warm_up(&self) {
        if self.warming.replace(false) {
            self.hide();
            if !self.keep_in.get() {
                self.panel.setAlphaValue(1.0);
                self.panel.setIgnoresMouseEvents(false);
            }
        }
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
        // cacheDisplay cannot capture layers: draw everything in `drawRect:` for the snapshot.
        let tree = self.view.ivars().use_layers.replace(false);
        self.ivars_mut(|st| st.snapshot_backdrop = true);
        let rep = content.bitmapImageRepForCachingDisplayInRect(bounds);
        if let Some(rep) = &rep {
            content.cacheDisplayInRect_toBitmapImageRep(bounds, rep);
        }
        self.ivars_mut(|st| st.snapshot_backdrop = false);
        self.view.ivars().use_layers.set(tree);
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
