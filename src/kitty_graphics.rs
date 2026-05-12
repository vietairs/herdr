use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use ratatui::layout::Rect;

use crate::app::state::AppState;
use crate::app::Mode;
use crate::ghostty::{KittyImageFormat, KittyImagePlacement};
use crate::layout::PaneId;

const KITTY_CHUNK_BYTES: usize = 3072;
const HOST_IMAGE_ID_BASE: u32 = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCellSize {
    pub width_px: u32,
    pub height_px: u32,
}

impl HostCellSize {
    pub(crate) fn from_terminal(area: Rect) -> Self {
        let Ok(size) = crossterm::terminal::window_size() else {
            return Self::fallback_for_area(area);
        };
        if size.columns == 0 || size.rows == 0 {
            return Self::fallback_for_area(area);
        }
        if size.width == 0 || size.height == 0 {
            return Self::fallback_for_area(area);
        }
        Self {
            width_px: (size.width as u32 / size.columns as u32).max(1),
            height_px: (size.height as u32 / size.rows as u32).max(1),
        }
        .for_area(area)
    }

    pub(crate) fn is_known(self) -> bool {
        self.width_px > 0 && self.height_px > 0
    }

    fn fallback_for_area(area: Rect) -> Self {
        Self {
            width_px: 8,
            height_px: 16,
        }
        .for_area(area)
    }

    fn for_area(self, area: Rect) -> Self {
        if area.width == 0 || area.height == 0 {
            return Self::default();
        }
        self
    }
}

#[derive(Debug)]
struct HostPlacement {
    pane_id: PaneId,
    area: Rect,
    cell_size: HostCellSize,
    placement: KittyImagePlacement,
    scrollback_offset: u32,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct ImageSignature {
    image_width: u32,
    image_height: u32,
    format_code: u32,
    data_len: usize,
    data_hash: u64,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PlacementSignature {
    x: u16,
    y: u16,
    cols: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClippedPlacement {
    x: u16,
    y: u16,
    cols: u32,
    rows: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    x_offset: u32,
    y_offset: u32,
}

static HOST_IMAGES: OnceLock<Mutex<HashMap<u32, ImageSignature>>> = OnceLock::new();
static HOST_PLACEMENTS: OnceLock<Mutex<HashMap<(u32, u32), PlacementSignature>>> = OnceLock::new();

pub(crate) fn paint_local_pane_graphics(app: &AppState, cell_size: HostCellSize) -> io::Result<()> {
    let mode_ok = app.mode == Mode::Terminal;
    let cell_ok = cell_size.is_known();
    tracing::debug!(
        mode_ok,
        cell_ok,
        cell_width_px = cell_size.width_px,
        cell_height_px = cell_size.height_px,
        active = ?app.active,
        pane_infos_len = app.view.pane_infos.len(),
        "paint_local_pane_graphics entry"
    );
    if !mode_ok || !cell_ok {
        tracing::debug!(
            reason = if !mode_ok {
                "not terminal mode"
            } else {
                "cell size unknown"
            },
            "paint_local_pane_graphics early return"
        );
        return clear_all_host_images();
    }

    let placements = collect_visible_placements(app, cell_size);
    tracing::debug!(
        placements_collected = placements.len(),
        "collect_visible_placements result"
    );
    let mut bytes = Vec::new();
    let mut current_placements = HashSet::new();

    let image_state = HOST_IMAGES.get_or_init(|| Mutex::new(HashMap::new()));
    let placement_state = HOST_PLACEMENTS.get_or_init(|| Mutex::new(HashMap::new()));
    if let (Ok(mut host_images), Ok(mut host_placements)) =
        (image_state.lock(), placement_state.lock())
    {
        for placement in &placements {
            let clipped = clipped_placement(placement);
            tracing::debug!(
                pane_id = ?placement.pane_id,
                has_clipped = clipped.is_some(),
                grid_cols = placement.placement.render.grid_cols,
                grid_rows = placement.placement.render.grid_rows,
                viewport_col = placement.placement.render.viewport_col,
                viewport_row = placement.placement.render.viewport_row,
                area_w = placement.area.width,
                area_h = placement.area.height,
                "clipped_placement result"
            );
            let Some((clipped, format_code)) = clipped else {
                continue;
            };
            let host_id = host_image_id(placement.pane_id, &placement.placement);
            let host_placement_id = host_placement_id(placement.pane_id, &placement.placement);
            let image_signature = image_signature(placement, format_code);
            let placement_signature = placement_signature(clipped);
            let placement_key = (host_id, host_placement_id);
            current_placements.insert(placement_key);

            match host_images.get(&host_id).copied() {
                Some(existing) if existing == image_signature => {}
                Some(_) => {
                    encode_delete_image(&mut bytes, host_id);
                    host_placements.retain(|(image_id, _), _| *image_id != host_id);
                    encode_upload_image(&mut bytes, placement, format_code, host_id);
                    host_images.insert(host_id, image_signature);
                }
                None => {
                    encode_upload_image(&mut bytes, placement, format_code, host_id);
                    host_images.insert(host_id, image_signature);
                }
            }

            match host_placements.get_mut(&placement_key) {
                Some(existing) if *existing == placement_signature => {}
                Some(existing) => {
                    encode_display_placement(
                        &mut bytes,
                        clipped,
                        host_id,
                        host_placement_id,
                        placement.placement.z,
                    );
                    *existing = placement_signature;
                }
                None => {
                    encode_display_placement(
                        &mut bytes,
                        clipped,
                        host_id,
                        host_placement_id,
                        placement.placement.z,
                    );
                    host_placements.insert(placement_key, placement_signature);
                }
            }
        }

        let stale_placements: Vec<(u32, u32)> = host_placements
            .keys()
            .copied()
            .filter(|key| !current_placements.contains(key))
            .collect();
        for (host_id, host_placement_id) in stale_placements {
            encode_delete_placement(&mut bytes, host_id, host_placement_id);
            host_placements.remove(&(host_id, host_placement_id));
        }
    }

    if bytes.is_empty() {
        return Ok(());
    }

    tracing::debug!(
        placements = placements.len(),
        bytes = bytes.len(),
        cell_width_px = cell_size.width_px,
        cell_height_px = cell_size.height_px,
        "painting kitty graphics placements"
    );

    let mut framed = Vec::with_capacity(bytes.len() + 8);
    framed.extend_from_slice(b"\x1b7");
    framed.extend_from_slice(&bytes);
    framed.extend_from_slice(b"\x1b8");

    let mut stdout = io::stdout().lock();
    stdout.write_all(&framed)?;
    stdout.flush()
}

fn clear_all_host_images() -> io::Result<()> {
    let image_state = HOST_IMAGES.get_or_init(|| Mutex::new(HashMap::new()));
    let placement_state = HOST_PLACEMENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut bytes = Vec::new();
    if let (Ok(mut host_images), Ok(mut host_placements)) =
        (image_state.lock(), placement_state.lock())
    {
        for id in host_images.keys().copied().collect::<Vec<_>>() {
            encode_delete_image(&mut bytes, id);
        }
        host_images.clear();
        host_placements.clear();
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()
}

fn collect_visible_placements(app: &AppState, cell_size: HostCellSize) -> Vec<HostPlacement> {
    let ws_idx = match app.active {
        Some(idx) => idx,
        None => {
            tracing::debug!("collect_visible_placements: no active workspace");
            return Vec::new();
        }
    };
    let tab = match app
        .workspaces
        .get(ws_idx)
        .and_then(crate::workspace::Workspace::active_tab)
    {
        Some(t) => t,
        None => {
            tracing::debug!(ws_idx, "collect_visible_placements: no active tab");
            return Vec::new();
        }
    };

    tracing::debug!(
        ws_idx,
        tab_runtimes_len = tab.runtimes.len(),
        pane_infos_len = app.view.pane_infos.len(),
        "collect_visible_placements: starting iteration"
    );
    let mut placements = Vec::new();
    for info in &app.view.pane_infos {
        let runtime = match tab.runtimes.get(&info.id) {
            Some(rt) => rt,
            None => {
                tracing::debug!(pane_id = ?info.id, "collect_visible_placements: runtime not found");
                continue;
            }
        };
        for placement in runtime.kitty_image_placements() {
            let scrollback_offset = runtime
                .scroll_metrics()
                .map(|m| m.offset_from_bottom as u32)
                .unwrap_or(0);
            placements.push(HostPlacement {
                pane_id: info.id,
                area: info.inner_rect,
                cell_size,
                placement,
                scrollback_offset,
            });
        }
    }
    tracing::debug!(
        placements_len = placements.len(),
        "collect_visible_placements: done"
    );
    placements
}

fn host_image_id(pane_id: PaneId, placement: &KittyImagePlacement) -> u32 {
    let mut hasher = DefaultHasher::new();
    pane_id.raw().hash(&mut hasher);
    placement.image_id.hash(&mut hasher);
    HOST_IMAGE_ID_BASE + ((hasher.finish() as u32) % 900_000)
}

fn host_placement_id(pane_id: PaneId, placement: &KittyImagePlacement) -> u32 {
    let mut hasher = DefaultHasher::new();
    pane_id.raw().hash(&mut hasher);
    placement.image_id.hash(&mut hasher);
    placement.placement_id.hash(&mut hasher);
    1 + ((hasher.finish() as u32) % 900_000)
}

fn encode_delete_image(out: &mut Vec<u8>, id: u32) {
    let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2;\x1b\\");
}

fn encode_delete_placement(out: &mut Vec<u8>, host_id: u32, host_placement_id: u32) {
    let _ = write!(
        out,
        "\x1b_Ga=d,d=i,i={host_id},p={host_placement_id},q=2;\x1b\\"
    );
}

fn encode_upload_image(
    out: &mut Vec<u8>,
    placement: &HostPlacement,
    format_code: u32,
    host_id: u32,
) {
    if placement.placement.data.is_empty() {
        return;
    }

    let control = format!(
        "a=t,t=d,f={format_code},s={},v={},i={host_id},q=2",
        placement.placement.image_width, placement.placement.image_height,
    );
    encode_kitty_data(out, &control, &placement.placement.data);
}

fn encode_display_placement(
    out: &mut Vec<u8>,
    clipped: ClippedPlacement,
    host_id: u32,
    host_placement_id: u32,
    z: i32,
) {
    let _ = write!(out, "\x1b[{};{}H", clipped.y + 1, clipped.x + 1);
    let mut control = format!(
        "a=p,i={host_id},p={host_placement_id},c={},r={},z={z},C=1,q=2",
        clipped.cols, clipped.rows,
    );
    if clipped.source_x > 0 {
        let _ = write!(control, ",x={}", clipped.source_x);
    }
    if clipped.source_y > 0 {
        let _ = write!(control, ",y={}", clipped.source_y);
    }
    if clipped.source_width > 0 {
        let _ = write!(control, ",w={}", clipped.source_width);
    }
    if clipped.source_height > 0 {
        let _ = write!(control, ",h={}", clipped.source_height);
    }
    if clipped.x_offset > 0 {
        let _ = write!(control, ",X={}", clipped.x_offset);
    }
    if clipped.y_offset > 0 {
        let _ = write!(control, ",Y={}", clipped.y_offset);
    }

    let _ = write!(out, "\x1b_G{control};\x1b\\");
}

fn clipped_placement(placement: &HostPlacement) -> Option<(ClippedPlacement, u32)> {
    if placement.area.width == 0 || placement.area.height == 0 {
        tracing::debug!(
            area_w = placement.area.width,
            area_h = placement.area.height,
            "clipped_placement: area zero"
        );
        return None;
    }
    let render = placement.placement.render;
    if render.grid_cols == 0 || render.grid_rows == 0 {
        tracing::debug!(
            grid_cols = render.grid_cols,
            grid_rows = render.grid_rows,
            "clipped_placement: grid zero"
        );
        return None;
    }
    let format_code = kitty_format_code(placement.placement.format);

    let left_clip_cells = if render.viewport_col < 0 {
        render.viewport_col.saturating_neg() as u32
    } else {
        0
    };
    let top_clip_cells = if render.viewport_row < 0 {
        render.viewport_row.saturating_neg() as u32
    } else {
        0
    };
    let viewport_col = render.viewport_col.max(0) as u32;
    let viewport_row = render.viewport_row.max(0) as u32;
    tracing::debug!(
        viewport_col = viewport_col,
        viewport_row = viewport_row,
        area_w = placement.area.width,
        area_h = placement.area.height,
        scrollback_offset = placement.scrollback_offset,
        raw_viewport_row = render.viewport_row,
        cond1 = viewport_col >= placement.area.width as u32,
        cond2 = viewport_row >= placement.area.height as u32,
        "clipped_placement: viewport check"
    );
    if viewport_col >= placement.area.width as u32 || viewport_row >= placement.area.height as u32 {
        return None;
    }

    let visible_cols = render
        .grid_cols
        .saturating_sub(left_clip_cells)
        .min(placement.area.width as u32 - viewport_col);
    let visible_rows = render
        .grid_rows
        .saturating_sub(top_clip_cells)
        .min(placement.area.height as u32 - viewport_row);
    tracing::debug!(
        visible_cols = visible_cols,
        visible_rows = visible_rows,
        left_clip_cells = left_clip_cells,
        top_clip_cells = top_clip_cells,
        "clipped_placement: visible dims check"
    );
    if visible_cols == 0 || visible_rows == 0 {
        return None;
    }

    let source_width = if render.source_width == 0 {
        placement.placement.image_width
    } else {
        render.source_width
    };
    let source_height = if render.source_height == 0 {
        placement.placement.image_height
    } else {
        render.source_height
    };
    let pixel_width = render
        .pixel_width
        .max(
            render
                .grid_cols
                .saturating_mul(placement.cell_size.width_px),
        )
        .max(1);
    let pixel_height = render
        .pixel_height
        .max(
            render
                .grid_rows
                .saturating_mul(placement.cell_size.height_px),
        )
        .max(1);

    let crop_left_px = left_clip_cells.saturating_mul(placement.cell_size.width_px);
    let crop_top_px = top_clip_cells.saturating_mul(placement.cell_size.height_px);
    let visible_width_px = visible_cols.saturating_mul(placement.cell_size.width_px);
    let visible_height_px = visible_rows.saturating_mul(placement.cell_size.height_px);

    let source_x = render.source_x + scale_pixels(crop_left_px, source_width, pixel_width);
    let source_y = render.source_y + scale_pixels(crop_top_px, source_height, pixel_height);
    let source_width = scale_pixels(visible_width_px, source_width, pixel_width)
        .max(1)
        .min(placement.placement.image_width.saturating_sub(source_x));
    let source_height = scale_pixels(visible_height_px, source_height, pixel_height)
        .max(1)
        .min(placement.placement.image_height.saturating_sub(source_y));

    if source_width == 0 || source_height == 0 {
        tracing::debug!(
            source_width = source_width,
            source_height = source_height,
            image_width = placement.placement.image_width,
            image_height = placement.placement.image_height,
            "clipped_placement: source dims zero"
        );
        return None;
    }

    tracing::debug!("clipped_placement: success");
    Some((
        ClippedPlacement {
            x: placement.area.x + viewport_col as u16,
            y: placement.area.y + viewport_row as u16,
            cols: visible_cols,
            rows: visible_rows,
            source_x,
            source_y,
            source_width,
            source_height,
            x_offset: if left_clip_cells == 0 {
                placement.placement.x_offset
            } else {
                0
            },
            y_offset: if top_clip_cells == 0 {
                placement.placement.y_offset
            } else {
                0
            },
        },
        format_code,
    ))
}

fn scale_pixels(value: u32, source: u32, dest: u32) -> u32 {
    ((value as u64).saturating_mul(source as u64) / dest.max(1) as u64).min(u32::MAX as u64) as u32
}

fn image_signature(placement: &HostPlacement, format_code: u32) -> ImageSignature {
    let mut hasher = DefaultHasher::new();
    placement.placement.data.hash(&mut hasher);
    ImageSignature {
        image_width: placement.placement.image_width,
        image_height: placement.placement.image_height,
        format_code,
        data_len: placement.placement.data.len(),
        data_hash: hasher.finish(),
    }
}

fn placement_signature(clipped: ClippedPlacement) -> PlacementSignature {
    PlacementSignature {
        x: clipped.x,
        y: clipped.y,
        cols: clipped.cols,
        rows: clipped.rows,
        source_x: clipped.source_x,
        source_y: clipped.source_y,
        source_width: clipped.source_width,
        source_height: clipped.source_height,
        x_offset: clipped.x_offset,
        y_offset: clipped.y_offset,
    }
}

fn kitty_format_code(format: KittyImageFormat) -> u32 {
    match format {
        KittyImageFormat::Rgb => 24,
        KittyImageFormat::Rgba => 32,
        KittyImageFormat::Png => 100,
    }
}

fn encode_kitty_data(out: &mut Vec<u8>, control: &str, data: &[u8]) {
    let mut chunks = data.chunks(KITTY_CHUNK_BYTES).peekable();
    let Some(first) = chunks.next() else {
        return;
    };
    let more = if chunks.peek().is_some() { 1 } else { 0 };
    let encoded = base64::engine::general_purpose::STANDARD.encode(first);
    let _ = write!(out, "\x1b_G{control},m={more};{encoded}\x1b\\");

    while let Some(chunk) = chunks.next() {
        let more = if chunks.peek().is_some() { 1 } else { 0 };
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        let _ = write!(out, "\x1b_Gm={more};{encoded}\x1b\\");
    }
}
