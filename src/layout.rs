//! BSP tree layout for tiling panes within a workspace.

use std::cmp::Reverse;

use ratatui::{
    layout::{Direction, Rect},
    widgets::Borders,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneId(u32);

/// Global atomic counter for unique PaneId generation across all workspaces.
static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

impl PaneId {
    /// Allocate a globally unique PaneId.
    pub fn alloc() -> Self {
        Self(NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// Reconstruct from a saved u32 (persistence only).
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// Snapshot of a pane's position and focus state after layout.
#[derive(Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    /// Outer rect (including borders if present).
    pub rect: Rect,
    /// Inner rect (content area, excluding borders). Used for selection.
    pub inner_rect: Rect,
    /// Visible scrollbar lane, when scrollback is present. `inner_rect` may still
    /// exclude a stable hidden gutter when this is `None`.
    pub scrollbar_rect: Option<Rect>,
    /// Borders drawn around this pane after UI chrome is applied.
    pub borders: Borders,
    pub is_focused: bool,
}

/// Info about a split boundary, used for mouse drag resize.
#[derive(Clone)]
pub struct SplitBorder {
    /// Position of the divider line (x for horizontal split, y for vertical).
    pub pos: u16,
    /// Direction of the split that created this border.
    pub direction: Direction,
    /// Ratio assigned to the first child of this split.
    pub ratio: f32,
    /// Total area of the split node.
    pub area: Rect,
    /// Path from root to this split node (false=first, true=second).
    pub path: Vec<bool>,
}

/// Cardinal direction for pane navigation.
#[derive(Debug, Clone, Copy)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the BSP tree. Public for serialization.
pub enum Node {
    Pane(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

/// BSP tiling layout. Tracks a tree of splits and a focused pane.
pub struct TileLayout {
    root: Node,
    focus: PaneId,
}

impl TileLayout {
    /// Create a new layout with a single pane (globally unique ID).
    /// Returns (layout, root_pane_id) so the caller can create the pane.
    pub fn new() -> (Self, PaneId) {
        let root_id = PaneId::alloc();
        (
            Self {
                root: Node::Pane(root_id),
                focus: root_id,
            },
            root_id,
        )
    }

    pub fn focused(&self) -> PaneId {
        self.focus
    }

    pub fn pane_count(&self) -> usize {
        count_panes(&self.root)
    }

    /// Compute rects for all panes given the available area.
    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut result = Vec::new();
        collect_panes(&self.root, area, self.focus, &mut result);
        result
    }

    /// Collect all split boundaries for mouse drag resize.
    pub fn splits(&self, area: Rect) -> Vec<SplitBorder> {
        let mut result = Vec::new();
        collect_splits(&self.root, area, vec![], &mut result);
        result
    }

    /// Split the focused pane. Returns the new pane's id.
    pub fn split_focused(&mut self, direction: Direction) -> PaneId {
        self.split_focused_with_ratio(direction, 0.5)
    }

    /// Split the focused pane with a custom first-child ratio.
    pub fn split_focused_with_ratio(&mut self, direction: Direction, ratio: f32) -> PaneId {
        let new_id = PaneId::alloc();
        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, self.focus, direction, new_id, valid_split_ratio(ratio));
        self.focus = new_id;
        new_id
    }

    /// Insert an existing pane id next to a target pane without allocating a new
    /// pane or spawning a terminal runtime.
    pub fn insert_pane_near(
        &mut self,
        target: PaneId,
        moved: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> bool {
        if target == moved {
            return false;
        }
        let ids = self.pane_ids();
        if !ids.contains(&target) || ids.contains(&moved) {
            return false;
        }

        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        self.root = split_at(old, target, direction, moved, valid_split_ratio(ratio));
        self.focus = moved;
        true
    }

    /// Close the focused pane. Returns false if it's the last pane.
    pub fn close_focused(&mut self) -> bool {
        if self.pane_count() <= 1 {
            return false;
        }
        let target = self.focus;
        let ids = self.pane_ids();
        let pos = ids.iter().position(|id| *id == target).unwrap();
        let new_focus = if pos + 1 < ids.len() {
            ids[pos + 1]
        } else {
            ids[pos - 1]
        };
        let placeholder = PaneId::from_raw(0);
        let old = std::mem::replace(&mut self.root, Node::Pane(placeholder));
        if let Some(new_root) = remove_pane(old, target) {
            self.root = new_root;
            self.focus = new_focus;
            true
        } else {
            false
        }
    }

    pub fn focus_pane(&mut self, id: PaneId) {
        if self.pane_ids().contains(&id) {
            self.focus = id;
        }
    }

    /// Swap two pane ids in the layout tree while preserving split shape and
    /// ratios. Returns true only when both panes exist and are different.
    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second {
            return false;
        }
        let ids = self.pane_ids();
        if !ids.contains(&first) || !ids.contains(&second) {
            return false;
        }
        swap_pane_ids(&mut self.root, first, second);
        true
    }

    /// Set the ratio of a split node at the given path.
    pub fn set_ratio_at(&mut self, path: &[bool], ratio: f32) -> bool {
        set_ratio_at(&mut self.root, path, ratio.clamp(0.1, 0.9))
    }

    /// Adjust the nearest split in the given direction for the focused pane.
    /// `delta` is positive to grow, negative to shrink.
    pub fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        let panes = self.panes(area);
        let Some(focused) = panes.iter().find(|p| p.is_focused) else {
            return;
        };
        let focused_rect = focused.rect;
        let splits = self.splits(area);

        let target_dir = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);

        let best = nearest_resize_split(&splits, target_dir, focused_rect, nav).or_else(|| {
            nearest_resize_split(&splits, target_dir, focused_rect, opposite_direction(nav))
        });

        if let Some(split) = best {
            let path = split.path.clone();
            let current_ratio = get_ratio_at(&self.root, &path).unwrap_or(0.5);
            let adj = if grows { delta } else { -delta };
            self.set_ratio_at(&path, current_ratio + adj);
        }
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        nav: NavDirection,
        delta: f32,
        area: Rect,
    ) -> bool {
        if !self.pane_ids().contains(&pane_id) {
            return false;
        }
        let before = split_ratios(&self.root);
        let previous_focus = self.focus;
        self.focus = pane_id;
        self.resize_focused(nav, delta, area);
        self.focus = previous_focus;
        split_ratios(&self.root) != before
    }

    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        collect_ids(&self.root, &mut ids);
        ids
    }

    /// Access the tree root for serialization.
    pub fn root(&self) -> &Node {
        &self.root
    }

    /// Reconstruct a layout from a saved tree.
    /// Reconstruct a layout from a saved tree.
    pub fn from_saved(root: Node, focus: PaneId) -> Self {
        Self { root, focus }
    }

    /// Locate the path (branch choices from root) to the leaf holding `pane_id`.
    /// Matches the `SplitBorder.path` / `set_ratio_at` convention: `false` means
    /// take the `first` child at that `Split`, `true` means `second`. `None` if
    /// `pane_id` isn't in this tree.
    pub fn path_to_pane(&self, pane_id: PaneId) -> Option<Vec<bool>> {
        let mut path = Vec::new();
        if find_path_to_pane(&self.root, pane_id, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    /// Rebalance every split ratio in the whole tree so each leaf gets an
    /// equal share of area, weighted by leaf count rather than tree shape.
    /// No-op on a single-pane tree.
    pub fn balance_areas(&mut self) {
        balance_subtree_areas(&mut self.root);
    }

    /// Rebalance only the split ratios `path` walks through, from root down
    /// to (but not past) the node `path` addresses. Used for auto-resize:
    /// touching only the ancestor chain of a split/close leaves sibling
    /// subtrees untouched. Tolerates a `path` that runs past the current
    /// tree shape (e.g. captured before a `remove_pane` collapse) by
    /// stopping silently instead of panicking. Returns `true` if any ratio
    /// changed.
    pub fn balance_areas_along_path(&mut self, path: &[bool]) -> bool {
        balance_split_ratios_along_path(&mut self.root, path)
    }

    /// Rebalance the split ratios that outlive removing the pane addressed by
    /// `removed_path`, which must have been captured BEFORE `remove_pane` ran.
    ///
    /// `remove_pane` collapses the removed pane's parent split and promotes
    /// its sibling into that slot, so a pre-removal path no longer addresses
    /// the same nodes. A path of length `L` addresses `L + 1` nodes and the
    /// removed pane's parent sat at depth `L - 1`, so the ancestors that
    /// survive the collapse are exactly the length `L - 2` prefix.
    ///
    /// When `L < 2` the parent WAS the root, so no ancestor survives and
    /// nothing may be rebalanced. That case must not fall through to an empty
    /// path: an empty path does not mean "nothing", it means "balance the
    /// root" -- which after the collapse is the promoted sibling, a subtree
    /// the removed pane was never under. Returns `true` if any ratio changed.
    pub fn balance_areas_after_removal(&mut self, removed_path: &[bool]) -> bool {
        let Some(surviving_len) = removed_path.len().checked_sub(2) else {
            return false;
        };
        self.balance_areas_along_path(&removed_path[..surviving_len])
    }
}

// --- Directional pane navigation ---

/// Find the nearest pane in the given direction from `focused`.
pub fn find_in_direction(
    focused: &PaneInfo,
    direction: NavDirection,
    panes: &[PaneInfo],
) -> Option<PaneId> {
    let fr = focused.rect;

    panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.id != focused.id)
        .filter(|(_, p)| {
            let r = p.rect;
            match direction {
                NavDirection::Left => {
                    r.x + r.width <= fr.x && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Right => {
                    r.x >= fr.x + fr.width && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up => {
                    r.y + r.height <= fr.y && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
                NavDirection::Down => {
                    r.y >= fr.y + fr.height && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
            }
        })
        .min_by_key(|(index, p)| {
            let r = p.rect;
            let edge_distance = match direction {
                NavDirection::Left => fr.x.saturating_sub(r.x + r.width),
                NavDirection::Right => r.x.saturating_sub(fr.x + fr.width),
                NavDirection::Up => fr.y.saturating_sub(r.y + r.height),
                NavDirection::Down => r.y.saturating_sub(fr.y + fr.height),
            };
            let overlap = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_overlap_amount(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_overlap_amount(r.x, r.width, fr.x, fr.width)
                }
            };
            let center_distance = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_center_distance(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_center_distance(r.x, r.width, fr.x, fr.width)
                }
            };
            (edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, p)| p.id)
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start + b_len && a_start + a_len > b_start
}

fn split_on_requested_edge(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    split_edge_distance(split, focused, nav) <= 1
}

fn split_area_overlaps_focused_pane(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    match nav {
        NavDirection::Left | NavDirection::Right => {
            ranges_overlap(split.area.y, split.area.height, focused.y, focused.height)
        }
        NavDirection::Up | NavDirection::Down => {
            ranges_overlap(split.area.x, split.area.width, focused.x, focused.width)
        }
    }
}

fn nearest_resize_split(
    splits: &[SplitBorder],
    target_dir: Direction,
    focused: Rect,
    nav: NavDirection,
) -> Option<&SplitBorder> {
    splits
        .iter()
        .filter(|s| s.direction == target_dir)
        .filter(|s| split_area_overlaps_focused_pane(s, focused, nav))
        .filter(|s| split_on_requested_edge(s, focused, nav))
        .min_by_key(|s| split_edge_distance(s, focused, nav))
}

fn opposite_direction(nav: NavDirection) -> NavDirection {
    match nav {
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
    }
}

fn split_edge_distance(split: &SplitBorder, focused: Rect, nav: NavDirection) -> u32 {
    match nav {
        NavDirection::Left => (split.pos as i32 - focused.x as i32).unsigned_abs(),
        NavDirection::Right => {
            (split.pos as i32 - (focused.x + focused.width) as i32).unsigned_abs()
        }
        NavDirection::Up => (split.pos as i32 - focused.y as i32).unsigned_abs(),
        NavDirection::Down => {
            (split.pos as i32 - (focused.y + focused.height) as i32).unsigned_abs()
        }
    }
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_center = a_start.saturating_mul(2).saturating_add(a_len);
    let b_center = b_start.saturating_mul(2).saturating_add(b_len);
    a_center.abs_diff(b_center)
}

// --- Tree operations ---

fn count_panes(node: &Node) -> usize {
    match node {
        Node::Pane(_) => 1,
        Node::Split { first, second, .. } => count_panes(first) + count_panes(second),
    }
}

fn collect_panes(node: &Node, area: Rect, focus: PaneId, result: &mut Vec<PaneInfo>) {
    match node {
        Node::Pane(id) => {
            result.push(PaneInfo {
                id: *id,
                rect: area,
                // inner_rect is set during render when we know if borders are shown
                inner_rect: area,
                scrollbar_rect: None,
                borders: Borders::NONE,
                is_focused: *id == focus,
            });
        }
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split_rect(area, *direction, *ratio);
            collect_panes(first, a, focus, result);
            collect_panes(second, b, focus, result);
        }
    }
}

fn collect_splits(node: &Node, area: Rect, path: Vec<bool>, result: &mut Vec<SplitBorder>) {
    if let Node::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (a, b) = split_rect(area, *direction, *ratio);
        let pos = match direction {
            Direction::Horizontal => a.x + a.width,
            Direction::Vertical => a.y + a.height,
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: *ratio,
            area,
            path: path.clone(),
        });
        let mut lp = path.clone();
        lp.push(false);
        collect_splits(first, a, lp, result);
        let mut rp = path;
        rp.push(true);
        collect_splits(second, b, rp, result);
    }
}

fn collect_ids(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids(first, ids);
            collect_ids(second, ids);
        }
    }
}

fn split_ratios(node: &Node) -> Vec<(Vec<bool>, f32)> {
    fn collect(node: &Node, path: &mut Vec<bool>, out: &mut Vec<(Vec<bool>, f32)>) {
        match node {
            Node::Pane(_) => {}
            Node::Split {
                ratio,
                first,
                second,
                ..
            } => {
                out.push((path.clone(), *ratio));
                path.push(false);
                collect(first, path, out);
                path.pop();
                path.push(true);
                collect(second, path, out);
                path.pop();
            }
        }
    }

    let mut out = Vec::new();
    collect(node, &mut Vec::new(), &mut out);
    out
}

fn swap_pane_ids(node: &mut Node, first: PaneId, second: PaneId) {
    match node {
        Node::Pane(id) if *id == first => *id = second,
        Node::Pane(id) if *id == second => *id = first,
        Node::Pane(_) => {}
        Node::Split {
            first: first_child,
            second: second_child,
            ..
        } => {
            swap_pane_ids(first_child, first, second);
            swap_pane_ids(second_child, first, second);
        }
    }
}

fn split_at(
    node: Node,
    target: PaneId,
    direction: Direction,
    new_id: PaneId,
    split_ratio: f32,
) -> Node {
    match node {
        Node::Pane(id) if id == target => Node::Split {
            direction,
            ratio: split_ratio,
            first: Box::new(Node::Pane(id)),
            second: Box::new(Node::Pane(new_id)),
        },
        Node::Pane(_) => node,
        Node::Split {
            direction: d,
            ratio,
            first,
            second,
        } => Node::Split {
            direction: d,
            ratio,
            first: Box::new(split_at(*first, target, direction, new_id, split_ratio)),
            second: Box::new(split_at(*second, target, direction, new_id, split_ratio)),
        },
    }
}

fn valid_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    }
}

fn remove_pane(node: Node, target: PaneId) -> Option<Node> {
    match node {
        Node::Pane(id) if id == target => None,
        Node::Pane(_) => Some(node),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => match (remove_pane(*first, target), remove_pane(*second, target)) {
            (None, Some(s)) => Some(s),
            (Some(f), None) => Some(f),
            (Some(f), Some(s)) => Some(Node::Split {
                direction,
                ratio,
                first: Box::new(f),
                second: Box::new(s),
            }),
            (None, None) => None,
        },
    }
}

fn set_ratio_at(node: &mut Node, path: &[bool], new_ratio: f32) -> bool {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            *ratio = new_ratio;
            true
        } else if path[0] {
            set_ratio_at(second, &path[1..], new_ratio)
        } else {
            set_ratio_at(first, &path[1..], new_ratio)
        }
    } else {
        false
    }
}

fn get_ratio_at(node: &Node, path: &[bool]) -> Option<f32> {
    if let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    {
        if path.is_empty() {
            Some(*ratio)
        } else if path[0] {
            get_ratio_at(second, &path[1..])
        } else {
            get_ratio_at(first, &path[1..])
        }
    } else {
        None
    }
}

fn split_rect(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
    match direction {
        Direction::Horizontal => {
            let first_w = ((area.width as f32) * ratio).round() as u16;
            let second_w = area.width.saturating_sub(first_w);
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, second_w, area.height),
            )
        }
        Direction::Vertical => {
            let first_h = ((area.height as f32) * ratio).round() as u16;
            let second_h = area.height.saturating_sub(first_h);
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, second_h),
            )
        }
    }
}

fn find_path_to_pane(node: &Node, target: PaneId, path: &mut Vec<bool>) -> bool {
    match node {
        Node::Pane(id) => *id == target,
        Node::Split { first, second, .. } => {
            path.push(false);
            if find_path_to_pane(first, target, path) {
                return true;
            }
            path.pop();
            path.push(true);
            if find_path_to_pane(second, target, path) {
                return true;
            }
            path.pop();
            false
        }
    }
}

/// Ratio that gives `first_leaves` out of `first_leaves + second_leaves` leaves
/// an equal share of area, clamped to the same 0.1..=0.9 range every other
/// ratio write in this file respects (see `valid_split_ratio`). When the true
/// equal-area ratio falls outside that range (e.g. 1 leaf against 10+ leaves,
/// true ratio ~0.0909), the result clamps to 0.1 rather than write a ratio
/// nothing else in the layout can round-trip -- this is an accepted,
/// documented degradation, not a bug.
fn equal_area_ratio(first_leaves: usize, second_leaves: usize) -> f32 {
    let total = (first_leaves + second_leaves) as f32;
    valid_split_ratio(first_leaves as f32 / total)
}

/// Recursively rebalance every split in `node` to equal-area ratios,
/// returning the leaf count of `node` so callers can weight ancestor splits.
/// Same recursion shape as `count_panes`, extended with a ratio-setting side
/// effect.
fn balance_subtree_areas(node: &mut Node) -> usize {
    match node {
        Node::Pane(_) => 1,
        Node::Split {
            ratio,
            first,
            second,
            ..
        } => {
            let first_leaves = balance_subtree_areas(first);
            let second_leaves = balance_subtree_areas(second);
            *ratio = equal_area_ratio(first_leaves, second_leaves);
            first_leaves + second_leaves
        }
    }
}

/// Rebalance only the splits `path` walks through, stopping as soon as
/// `path` is exhausted or the walk reaches a `Node::Pane`. Never descends
/// past what the current tree actually contains, so a stale/overlong `path`
/// (e.g. captured before a `remove_pane` collapse shortened the tree) is
/// tolerated rather than panicking.
fn balance_split_ratios_along_path(node: &mut Node, path: &[bool]) -> bool {
    let Node::Split {
        ratio,
        first,
        second,
        ..
    } = node
    else {
        return false;
    };

    let new_ratio = equal_area_ratio(count_panes(first), count_panes(second));
    let changed = (*ratio - new_ratio).abs() > f32::EPSILON;
    *ratio = new_ratio;

    let descended = match path.first() {
        Some(true) => balance_split_ratios_along_path(second, &path[1..]),
        Some(false) => balance_split_ratios_along_path(first, &path[1..]),
        None => false,
    };

    changed || descended
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u32) -> PaneId {
        PaneId::from_raw(id)
    }

    fn sample_layout() -> TileLayout {
        TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.3,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.6,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.4,
                        first: Box::new(Node::Pane(pane(3))),
                        second: Box::new(Node::Pane(pane(4))),
                    }),
                }),
            },
            pane(2),
        )
    }

    fn pane_rects(layout: &TileLayout) -> Vec<(PaneId, Rect)> {
        layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .map(|info| (info.id, info.rect))
            .collect()
    }

    fn pane_rect(layout: &TileLayout, pane_id: PaneId) -> Rect {
        pane_rects(layout)
            .into_iter()
            .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            .expect("pane should exist")
    }

    fn split_snapshot(layout: &TileLayout) -> Vec<(Direction, f32)> {
        fn collect(node: &Node, out: &mut Vec<(Direction, f32)>) {
            match node {
                Node::Pane(_) => {}
                Node::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    out.push((*direction, *ratio));
                    collect(first, out);
                    collect(second, out);
                }
            }
        }

        let mut out = Vec::new();
        collect(layout.root(), &mut out);
        out
    }

    #[test]
    fn swap_panes_exchanges_leaf_ids_without_changing_cells() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);

        assert!(layout.swap_panes(pane(2), pane(4)));

        assert_eq!(layout.pane_count(), 4);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), pane(2));

        let after_rects = pane_rects(&layout);
        assert_eq!(after_rects[0], before_rects[0]);
        assert_eq!(after_rects[1], (pane(4), before_rects[1].1));
        assert_eq!(after_rects[2], before_rects[2]);
        assert_eq!(after_rects[3], (pane(2), before_rects[3].1));
    }

    #[test]
    fn swap_panes_is_noop_for_same_or_missing_pane() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);
        let before_focus = layout.focused();

        assert!(!layout.swap_panes(pane(2), pane(2)));
        assert!(!layout.swap_panes(pane(2), pane(99)));
        assert!(!layout.swap_panes(pane(99), pane(2)));

        assert_eq!(pane_rects(&layout), before_rects);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), before_focus);
    }

    #[test]
    fn insert_existing_pane_near_target_preserves_existing_ids_and_focuses_moved_pane() {
        let (mut layout, root) = TileLayout::new();
        let moved = pane(99);

        assert!(layout.insert_pane_near(root, moved, Direction::Horizontal, 0.25));

        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.pane_ids(), vec![root, moved]);
        assert_eq!(layout.focused(), moved);
        let splits = split_snapshot(&layout);
        assert_eq!(splits, vec![(Direction::Horizontal, 0.25)]);
        assert_eq!(pane_rect(&layout, root), Rect::new(0, 0, 25, 40));
        assert_eq!(pane_rect(&layout, moved), Rect::new(25, 0, 75, 40));
    }

    #[test]
    fn split_focused_with_ratio_sets_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();
        layout.focus_pane(root);

        layout.split_focused_with_ratio(Direction::Horizontal, 0.333);

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_preserves_focus_and_reports_change() {
        let mut layout = sample_layout();
        let original_focus = layout.focused();

        assert!(layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_focused(Direction::Horizontal);
        layout.focus_pane(root);

        assert!(layout.resize_pane(right, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(left, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut horizontal, _left) = TileLayout::new();
        let right = horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(right, NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(top, NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut vertical, _top) = TileLayout::new();
        let bottom = vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(bottom, NavDirection::Down, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_outer_edge_falls_back_to_horizontal_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Vertical, 0.5));
    }

    #[test]
    fn resize_outer_edge_falls_back_to_vertical_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
    }

    #[test]
    fn resize_uses_split_in_same_branch_when_borders_share_coordinate() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(3),
        );

        assert!(layout.resize_pane(pane(3), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn find_in_direction_tiebreaks_by_larger_overlap_before_layout_order() {
        let focused = PaneInfo {
            id: pane(1),
            rect: Rect::new(10, 10, 10, 10),
            inner_rect: Rect::new(10, 10, 10, 10),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: true,
        };
        let small_overlap_first = PaneInfo {
            id: pane(2),
            rect: Rect::new(0, 10, 10, 2),
            inner_rect: Rect::new(0, 10, 10, 2),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
        };
        let larger_overlap_second = PaneInfo {
            id: pane(3),
            rect: Rect::new(0, 10, 10, 8),
            inner_rect: Rect::new(0, 10, 10, 8),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
        };
        let panes = vec![focused.clone(), small_overlap_first, larger_overlap_second];

        assert_eq!(
            find_in_direction(&focused, NavDirection::Left, &panes),
            Some(pane(3))
        );
    }

    /// Builds a right-leaning chain of `ids.len()` leaves: each split peels
    /// off `ids[0]` as `first` and nests the rest under `second`, cycling
    /// through `directions` (wrapping) for split direction. Every split
    /// starts at a skewed ratio (0.9) so tests can prove `balance_areas`
    /// corrects it rather than trivially matching an already-balanced tree.
    fn skewed_chain(ids: &[u32], directions: &[Direction]) -> Node {
        if ids.len() == 1 {
            return Node::Pane(pane(ids[0]));
        }
        let dir = directions[0];
        let rest_dirs = if directions.len() > 1 {
            &directions[1..]
        } else {
            directions
        };
        Node::Split {
            direction: dir,
            ratio: 0.9,
            first: Box::new(Node::Pane(pane(ids[0]))),
            second: Box::new(skewed_chain(&ids[1..], rest_dirs)),
        }
    }

    /// Builds a complete binary BSP tree over `ids.len()` leaves (must be a
    /// power of two), splitting the leaf set evenly at every level and
    /// cycling through `directions` (wrapping) per depth level. Every split
    /// starts at a skewed ratio (0.9) so tests can prove `balance_areas`
    /// corrects it. Unlike `skewed_chain`, leaf counts are equal on both
    /// sides of every split, so with power-of-two `Rect` dimensions every
    /// split divides evenly -- no rounding error compounds toward the
    /// leaves, which is what makes an exact-equal-area assertion valid here
    /// (a lopsided "1 vs rest" chain does not have this property: a 1-pixel
    /// rounding error near the leaves gets multiplied by whatever large
    /// dimension remains on the other axis).
    fn balanced_binary_tree(ids: &[u32], directions: &[Direction]) -> Node {
        if ids.len() == 1 {
            return Node::Pane(pane(ids[0]));
        }
        let mid = ids.len() / 2;
        let dir = directions[0];
        let rest_dirs = if directions.len() > 1 {
            &directions[1..]
        } else {
            directions
        };
        Node::Split {
            direction: dir,
            ratio: 0.9,
            first: Box::new(balanced_binary_tree(&ids[..mid], rest_dirs)),
            second: Box::new(balanced_binary_tree(&ids[mid..], rest_dirs)),
        }
    }

    fn leaf_areas(layout: &TileLayout, area: Rect) -> Vec<i64> {
        layout
            .panes(area)
            .into_iter()
            .map(|p| p.rect.width as i64 * p.rect.height as i64)
            .collect()
    }

    fn assert_areas_within_one_cell(areas: &[i64]) {
        let max = *areas.iter().max().expect("at least one leaf");
        let min = *areas.iter().min().expect("at least one leaf");
        assert!(
            max - min <= 1,
            "leaf areas should be equal within one cell of rounding: {areas:?}"
        );
    }

    // --- remove_pane characterization (pins current collapse behavior) ---

    #[test]
    fn remove_pane_discards_parent_split_ratio_on_collapse() {
        let sibling_subtree_ratio = 0.25;
        let tree = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.7,
            first: Box::new(Node::Split {
                direction: Direction::Vertical,
                ratio: sibling_subtree_ratio,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Pane(pane(2))),
            }),
            second: Box::new(Node::Pane(pane(3))),
        };

        let result = remove_pane(tree, pane(3)).expect("two panes should remain");

        match result {
            Node::Split { ratio, .. } => {
                assert!(
                    (ratio - sibling_subtree_ratio).abs() < f32::EPSILON,
                    "sibling subtree's ratio must survive the parent collapse unchanged"
                );
            }
            Node::Pane(_) => panic!("expected the sibling split subtree to be promoted"),
        }
    }

    #[test]
    fn remove_pane_of_last_child_promotes_sibling_ratio_unchanged() {
        let tree = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.3,
            first: Box::new(Node::Pane(pane(1))),
            second: Box::new(Node::Pane(pane(2))),
        };

        let result = remove_pane(tree, pane(2)).expect("one pane should remain");
        let layout = TileLayout::from_saved(result, pane(1));

        assert_eq!(layout.pane_count(), 1);
        assert_eq!(pane_rect(&layout, pane(1)), Rect::new(0, 0, 100, 40));
    }

    // --- path_to_pane ---

    #[test]
    fn path_to_pane_finds_leaf_at_each_depth() {
        let layout = sample_layout();
        assert_eq!(layout.path_to_pane(pane(1)), Some(vec![false]));
        assert_eq!(layout.path_to_pane(pane(2)), Some(vec![true, false]));
        assert_eq!(layout.path_to_pane(pane(3)), Some(vec![true, true, false]));
        assert_eq!(layout.path_to_pane(pane(4)), Some(vec![true, true, true]));
    }

    #[test]
    fn path_to_pane_missing_pane_returns_none() {
        let layout = sample_layout();
        assert_eq!(layout.path_to_pane(pane(99)), None);
    }

    // --- balance_areas ---

    #[test]
    fn balance_areas_equalizes_2x2_grid() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.3,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.7,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.2,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(1),
        );

        layout.balance_areas();

        assert_areas_within_one_cell(&leaf_areas(&layout, Rect::new(0, 0, 100, 40)));
    }

    #[test]
    fn balance_areas_equalizes_leaf_areas_at_depth_3() {
        let ids: Vec<u32> = (1..=8).collect();
        let mut layout = TileLayout::from_saved(
            balanced_binary_tree(
                &ids,
                &[
                    Direction::Horizontal,
                    Direction::Vertical,
                    Direction::Horizontal,
                ],
            ),
            pane(1),
        );

        layout.balance_areas();

        assert_areas_within_one_cell(&leaf_areas(&layout, Rect::new(0, 0, 128, 64)));
    }

    #[test]
    fn balance_areas_equalizes_leaf_areas_at_depth_4() {
        let ids: Vec<u32> = (1..=16).collect();
        let mut layout = TileLayout::from_saved(
            balanced_binary_tree(
                &ids,
                &[
                    Direction::Horizontal,
                    Direction::Vertical,
                    Direction::Horizontal,
                    Direction::Vertical,
                ],
            ),
            pane(1),
        );

        layout.balance_areas();

        assert_areas_within_one_cell(&leaf_areas(&layout, Rect::new(0, 0, 128, 64)));
    }

    #[test]
    fn balance_areas_weights_by_leaf_count_not_split_position() {
        // 2-leaf branch vs 1-leaf branch: equal-area ratio must be 2/3, not
        // the position-based 0.5 a naive split-count balance would produce.
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.9,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );

        layout.balance_areas();

        let splits = split_snapshot(&layout);
        assert!(
            (splits[0].1 - (2.0 / 3.0)).abs() < 1e-5,
            "root ratio should reflect 2 vs 1 leaves, not split position: {splits:?}"
        );
    }

    #[test]
    fn balance_areas_1v9_leaves_hits_exact_lower_clamp() {
        // 1 leaf vs 9 leaves: true equal-area ratio is 1/10 = 0.1 exactly,
        // which sits right at (and is representable within) the clamp floor.
        let ids: Vec<u32> = (1..=10).collect();
        let mut layout =
            TileLayout::from_saved(skewed_chain(&ids, &[Direction::Horizontal]), pane(1));

        layout.balance_areas();

        let splits = split_snapshot(&layout);
        assert!(
            (splits[0].1 - 0.1).abs() < f32::EPSILON,
            "1/10 is exactly representable and within the clamp range: {splits:?}"
        );
    }

    #[test]
    fn balance_areas_1v10_leaves_clamps_with_documented_error() {
        // 1 leaf vs 10 leaves: true equal-area ratio is 1/11 ~= 0.0909, below
        // the 0.1 minimum. This is the accepted degradation (see
        // `equal_area_ratio`): the result clamps to 0.1 rather than write an
        // unrepresentable ratio, so the "1" side ends up very slightly larger
        // than a perfect equal split would give it.
        let ids: Vec<u32> = (1..=11).collect();
        let mut layout =
            TileLayout::from_saved(skewed_chain(&ids, &[Direction::Horizontal]), pane(1));

        layout.balance_areas();

        let splits = split_snapshot(&layout);
        assert!(
            (splits[0].1 - 0.1).abs() < f32::EPSILON,
            "ratio should clamp to 0.1, not the true 1/11: {splits:?}"
        );
    }

    #[test]
    fn balance_areas_single_pane_is_noop() {
        let (mut layout, root) = TileLayout::new();

        layout.balance_areas();

        assert_eq!(layout.pane_count(), 1);
        assert_eq!(pane_rect(&layout, root), Rect::new(0, 0, 100, 40));
    }

    #[test]
    fn balance_areas_is_idempotent() {
        let mut layout = sample_layout();

        layout.balance_areas();
        let once = split_snapshot(&layout);
        layout.balance_areas();
        let twice = split_snapshot(&layout);

        assert_eq!(once, twice);
    }

    #[test]
    fn balance_areas_handles_deeply_nested_chain_without_stack_issues() {
        // 17 leaves = 16 nested split levels, matching MAX_LAYOUT_DEPTH.
        let ids: Vec<u32> = (1..=17).collect();
        let mut layout = TileLayout::from_saved(
            skewed_chain(&ids, &[Direction::Horizontal, Direction::Vertical]),
            pane(1),
        );

        layout.balance_areas();

        assert_eq!(layout.pane_count(), 17);
        assert_eq!(leaf_areas(&layout, Rect::new(0, 0, 100, 40)).len(), 17);
    }

    // --- balance_areas_along_path ---

    #[test]
    fn balance_areas_along_path_only_touches_path_nodes() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.5,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.3,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.3,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(1),
        );

        let path = layout.path_to_pane(pane(1)).expect("pane(1) exists");
        let changed = layout.balance_areas_along_path(&path);

        assert!(changed);
        let splits = split_snapshot(&layout);
        // splits[0] = root, splits[1] = subtree containing pane(1)
        // (rebalanced 0.3 -> 0.5), splits[2] = unrelated sibling subtree.
        assert!((splits[1].1 - 0.5).abs() < f32::EPSILON);
        assert!(
            (splits[2].1 - 0.3).abs() < f32::EPSILON,
            "sibling subtree ratio must be untouched by an unrelated path: {splits:?}"
        );
    }

    #[test]
    fn balance_areas_along_path_tolerates_path_longer_than_tree() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.2,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Pane(pane(2))),
            },
            pane(1),
        );

        // Simulates a path captured before a `remove_pane` collapse
        // shortened the tree: it addresses splits that no longer exist.
        let stale_path = vec![false, true, false];
        let changed = layout.balance_areas_along_path(&stale_path);

        assert!(changed, "the root split itself should still rebalance");
        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert!((splits[0].1 - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn balance_areas_along_path_empty_path_balances_root_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.2,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Pane(pane(3))),
                }),
            },
            pane(1),
        );

        let changed = layout.balance_areas_along_path(&[]);

        assert!(changed);
        let splits = split_snapshot(&layout);
        // root: 1 leaf vs 2 leaves -> 1/3; the nested split below is
        // untouched since an empty path stops after the root.
        assert!((splits[0].1 - (1.0 / 3.0)).abs() < 1e-5);
        assert!(
            (splits[1].1 - 0.5).abs() < f32::EPSILON,
            "unvisited nested split must be untouched: {splits:?}"
        );
    }

    #[test]
    fn balance_areas_along_path_single_pane_is_noop() {
        let (mut layout, _root) = TileLayout::new();

        let changed = layout.balance_areas_along_path(&[false, true]);

        assert!(!changed);
        assert_eq!(layout.pane_count(), 1);
    }

    // --- ancestor-chain-only invariant for post-removal rebalancing ---
    //
    // These sweep every tree shape rather than a hand-picked fixture. Two
    // earlier hand-picked fixtures each missed a shape where the removed
    // pane's ancestors and the promoted sibling overlap differently, so the
    // shape space itself is the thing worth covering.

    /// Structural skeleton used to enumerate tree shapes before pane IDs and
    /// ratios are attached. `Node` is deliberately not `Clone`, so shapes are
    /// generated in this cheap form and materialized once each.
    #[derive(Clone)]
    enum Shape {
        Leaf,
        Split(Box<Shape>, Box<Shape>),
    }

    /// Every binary tree shape with exactly `leaves` leaves.
    fn all_shapes(leaves: usize) -> Vec<Shape> {
        if leaves == 1 {
            return vec![Shape::Leaf];
        }
        let mut out = Vec::new();
        for split_at in 1..leaves {
            for first in all_shapes(split_at) {
                for second in all_shapes(leaves - split_at) {
                    out.push(Shape::Split(
                        Box::new(first.clone()),
                        Box::new(second.clone()),
                    ));
                }
            }
        }
        out
    }

    /// Ratios that are recognizable after the fact: none of them can be
    /// produced by `equal_area_ratio` for the leaf counts these trees reach,
    /// so a ratio surviving in the tree proves that split was left alone.
    const MARKER_RATIOS: [f32; 6] = [0.11, 0.13, 0.17, 0.19, 0.23, 0.29];

    fn materialize(shape: &Shape, next_pane: &mut u32, next_ratio: &mut usize) -> Node {
        match shape {
            Shape::Leaf => {
                *next_pane += 1;
                Node::Pane(pane(*next_pane))
            }
            Shape::Split(first, second) => {
                let ratio = MARKER_RATIOS[*next_ratio];
                *next_ratio += 1;
                let first = materialize(first, next_pane, next_ratio);
                let second = materialize(second, next_pane, next_ratio);
                Node::Split {
                    direction: Direction::Horizontal,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }
            }
        }
    }

    fn collect_ratios(node: &Node, out: &mut Vec<f32>) {
        if let Node::Split {
            ratio,
            first,
            second,
            ..
        } = node
        {
            out.push(*ratio);
            collect_ratios(first, out);
            collect_ratios(second, out);
        }
    }

    /// Marker ratios of every split that is NOT an ancestor of `path`.
    /// A split at depth `d` is an ancestor iff the first `d` branch choices
    /// match `path` and `d < path.len()`.
    fn non_ancestor_markers(node: &Node, path: &[bool], depth: usize, out: &mut Vec<f32>) {
        let Node::Split {
            ratio,
            first,
            second,
            ..
        } = node
        else {
            return;
        };
        let on_ancestor_chain = depth < path.len();
        if !on_ancestor_chain {
            out.push(*ratio);
        }
        let next = path.get(depth).copied();
        non_ancestor_markers(
            first,
            if next == Some(false) { path } else { &[] },
            depth + 1,
            out,
        );
        non_ancestor_markers(
            second,
            if next == Some(true) { path } else { &[] },
            depth + 1,
            out,
        );
    }

    #[test]
    fn balance_after_removal_never_touches_a_non_ancestor_split() {
        for leaf_count in 2..=5 {
            for (shape_idx, shape) in all_shapes(leaf_count).iter().enumerate() {
                for removed in 1..=leaf_count as u32 {
                    let mut next_pane = 0;
                    let mut next_ratio = 0;
                    let root = materialize(shape, &mut next_pane, &mut next_ratio);
                    let mut layout = TileLayout::from_saved(root, pane(removed));
                    let path = layout
                        .path_to_pane(pane(removed))
                        .expect("every materialized pane is in the tree");

                    let mut expected_survivors = Vec::new();
                    non_ancestor_markers(layout.root(), &path, 0, &mut expected_survivors);

                    if !layout.close_focused() {
                        continue; // last pane in the tab; nothing left to balance
                    }
                    layout.balance_areas_after_removal(&path);

                    let mut remaining = Vec::new();
                    collect_ratios(layout.root(), &mut remaining);
                    for marker in expected_survivors {
                        assert!(
                            remaining.iter().any(|r| (r - marker).abs() < f32::EPSILON),
                            "leaves={leaf_count} shape={shape_idx} removed=pane({removed}) \
                             path={path:?}: split with marker ratio {marker} is not an \
                             ancestor of the removed pane, so its ratio must survive; \
                             remaining ratios were {remaining:?}"
                        );
                    }

                    // Positive side: when an ancestor does survive the
                    // collapse (path length >= 2) the root is one of them and
                    // must actually have been rebalanced, so this test cannot
                    // be satisfied by a no-op implementation.
                    if path.len() >= 2 {
                        let Node::Split {
                            ratio,
                            first,
                            second,
                            ..
                        } = layout.root()
                        else {
                            panic!("a tree with a length>=2 path keeps a split root");
                        };
                        let expected = equal_area_ratio(count_panes(first), count_panes(second));
                        assert!(
                            (ratio - expected).abs() < f32::EPSILON,
                            "leaves={leaf_count} shape={shape_idx} removed=pane({removed}): \
                             surviving root ancestor should be rebalanced to {expected}, got {ratio}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn balance_after_removal_is_noop_when_no_ancestor_survives() {
        // Root-level close: the removed pane's parent IS the root, so the
        // promoted sibling keeps every ratio it had.
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.2,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.75,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Pane(pane(3))),
                }),
            },
            pane(1),
        );
        let path = layout.path_to_pane(pane(1)).expect("pane 1 is in the tree");
        assert_eq!(path.len(), 1);

        assert!(layout.close_focused(), "two panes should remain");
        let changed = layout.balance_areas_after_removal(&path);

        assert!(
            !changed,
            "no ancestor survives, so nothing may be rebalanced"
        );
        let Node::Split { ratio, .. } = layout.root() else {
            panic!("expected the promoted split to become the root");
        };
        assert!((ratio - 0.75).abs() < f32::EPSILON, "got {ratio}");
    }

    #[test]
    fn balance_after_removal_ignores_a_path_that_cannot_have_ancestors() {
        let (mut layout, _root) = TileLayout::new();

        assert!(!layout.balance_areas_after_removal(&[]));
        assert!(!layout.balance_areas_after_removal(&[true]));
    }
}
