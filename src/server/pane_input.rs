use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};

use crate::protocol::{AttachScrollDirection, AttachScrollSource, ClientPaneInputEvent};

pub(super) fn downgrade_ineligible_pixel_mouse(
    events: &mut [ClientPaneInputEvent],
    pixel_mouse: bool,
    runtime_size: (u16, u16),
    runtime_pixels: Option<(u32, u32)>,
) {
    let (runtime_rows, runtime_cols) = runtime_size;
    for event in events {
        let ClientPaneInputEvent::Mouse {
            position, geometry, ..
        } = event
        else {
            continue;
        };
        let crate::protocol::ClientMousePosition::Pixels { x, y, column, row } = *position else {
            continue;
        };
        let exact = pixel_mouse
            && geometry.is_some_and(|geometry| {
                (runtime_rows, runtime_cols) == (geometry.rows, geometry.cols)
                    && runtime_pixels == Some((geometry.width_px, geometry.height_px))
                    && column < geometry.cols
                    && row < geometry.rows
                    && x > 0
                    && y > 0
                    && x <= geometry.width_px
                    && y <= geometry.height_px
            });
        if !exact {
            *position = crate::protocol::ClientMousePosition::Cell { column, row };
            *geometry = None;
        }
    }
}

pub(super) fn terminal_attach_mouse_position(
    runtime: &crate::terminal::TerminalRuntime,
    terminal_size: (u16, u16),
    cell_size: crate::kitty_graphics::HostCellSize,
    pixel_mouse: bool,
    host_sgr_pixels_active: bool,
    position: crate::protocol::ClientMousePosition,
    geometry: Option<crate::protocol::ClientMouseGeometry>,
) -> Option<crate::protocol::ClientMousePosition> {
    let runtime_size = runtime.current_size();
    let cell_fallback = |column, row| {
        (column < runtime_size.1 && row < runtime_size.0)
            .then_some(crate::protocol::ClientMousePosition::Cell { column, row })
    };
    let (x, y, column, row) = match position {
        crate::protocol::ClientMousePosition::Cell { column, row } => {
            return cell_fallback(column, row);
        }
        crate::protocol::ClientMousePosition::Pixels { x, y, column, row } => (x, y, column, row),
    };
    let Some(geometry) = geometry else {
        return cell_fallback(column, row);
    };
    let host_geometry = crate::input::mouse::HostGeometry::new(
        geometry.cols,
        geometry.rows,
        geometry.width_px,
        geometry.height_px,
    )?;
    if host_geometry.cell(x, y) != Some((column, row)) {
        return None;
    }
    let exact = (|| {
        let average_width = (geometry.width_px / u32::from(geometry.cols)).max(1);
        let average_height = (geometry.height_px / u32::from(geometry.rows)).max(1);
        let (child_width_px, child_height_px) = runtime.pixel_size()?;
        if !pixel_mouse
            || !host_sgr_pixels_active
            || !runtime.sgr_pixel_mouse_enabled()
            || terminal_size != (geometry.cols, geometry.rows)
            || runtime_size != (geometry.rows, geometry.cols)
            || !cell_size.is_known()
            || average_width != cell_size.width_px
            || average_height != cell_size.height_px
        {
            return None;
        }
        let crate::input::mouse::Position::Pixels { x, y } = (crate::input::mouse::HostPixels {
            x,
            y,
            geometry: host_geometry,
        })
        .pane_position(
            ratatui::layout::Rect::new(0, 0, geometry.cols, geometry.rows),
            child_width_px,
            child_height_px,
        )?
        else {
            return None;
        };
        Some(crate::protocol::ClientMousePosition::Pixels { x, y, column, row })
    })();
    exact.or_else(|| cell_fallback(column, row))
}

pub(super) fn apply_terminal_attach_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    column: Option<u16>,
    row: Option<u16>,
    modifiers: u8,
) -> Result<(), String> {
    apply_scroll(
        runtime,
        source,
        direction,
        lines,
        crate::input::mouse::Position::Cell {
            column: column.unwrap_or(0),
            row: row.unwrap_or(0),
        },
        modifiers,
    )
}

fn apply_scroll(
    runtime: &crate::terminal::TerminalRuntime,
    source: AttachScrollSource,
    direction: AttachScrollDirection,
    lines: u16,
    position: crate::input::mouse::Position,
    modifiers: u8,
) -> Result<(), String> {
    let wheel_kind = match direction {
        AttachScrollDirection::Up => MouseEventKind::ScrollUp,
        AttachScrollDirection::Down => MouseEventKind::ScrollDown,
    };
    if let AttachScrollSource::PageKey { input } = source {
        let host_scroll = runtime
            .plain_page_keys_use_host_scrollback()
            .unwrap_or(false);
        if host_scroll {
            match direction {
                AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
                AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
            }
            return Ok(());
        }
        return apply_terminal_attach_input(runtime, input);
    }

    match runtime.wheel_routing() {
        Some(crate::pane::WheelRouting::MouseReport) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_mouse_wheel(
                wheel_kind,
                position,
                KeyModifiers::from_bits_truncate(modifiers),
            ) else {
                return Err(format!(
                    "failed to encode terminal attach mouse wheel event: {wheel_kind:?}"
                ));
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach mouse wheel input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::AlternateScroll) => {
            runtime.scroll_reset();
            let Some(bytes) = runtime.encode_alternate_scroll(wheel_kind) else {
                return Ok(());
            };
            runtime
                .try_send_bytes(Bytes::from(bytes))
                .map_err(|err| format!("terminal attach alternate scroll input failed: {err}"))?;
        }
        Some(crate::pane::WheelRouting::HostScroll) | None => match direction {
            AttachScrollDirection::Up => runtime.scroll_up(lines.max(1) as usize),
            AttachScrollDirection::Down => runtime.scroll_down(lines.max(1) as usize),
        },
    }
    Ok(())
}

pub(super) fn apply_terminal_attach_input(
    runtime: &crate::terminal::TerminalRuntime,
    data: Vec<u8>,
) -> Result<(), String> {
    runtime.scroll_reset();
    if let Some(text) = crate::raw_input::complete_text_bracketed_paste(&data) {
        runtime
            .try_send_paste(text.to_owned())
            .map_err(|err| format!("terminal attach paste failed: {err}"))
    } else {
        runtime
            .try_send_bytes(Bytes::from(data))
            .map_err(|err| format!("terminal attach input failed: {err}"))
    }
}

#[cfg(test)]
pub(crate) fn test_apply_client_pane_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
) -> Result<bool, String> {
    apply_client_pane_input_events(runtime, events)
}

pub(super) fn apply_client_pane_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
) -> Result<bool, String> {
    let mut keyboard_forwarded = false;
    apply_client_terminal_input_events(runtime, events, true, &mut keyboard_forwarded)?;
    Ok(keyboard_forwarded)
}

pub(super) fn apply_client_popup_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
) -> Result<bool, String> {
    let mut keyboard_forwarded = false;
    apply_client_terminal_input_events(runtime, events, false, &mut keyboard_forwarded)?;
    Ok(keyboard_forwarded)
}

/// Fits a pane-relative mouse position to the PTY grid `(rows, cols)`.
///
/// A client lays a pane out at its own surface size, which can be larger than
/// the PTY when another, smaller client bounds the tab's size: the extra cells
/// are blank padding with nothing behind them. A press or motion there has no
/// cell to land on and is dropped. A release, drag or wheel event is clamped to
/// the nearest cell instead, so a gesture that started inside the grid still
/// ends there and an application never sees a button stuck down.
pub(super) fn fit_mouse_position_to_grid(
    kind: MouseEventKind,
    position: crate::protocol::ClientMousePosition,
    (rows, cols): (u16, u16),
) -> Option<crate::protocol::ClientMousePosition> {
    let (column, row) = match position {
        crate::protocol::ClientMousePosition::Cell { column, row }
        | crate::protocol::ClientMousePosition::Pixels { column, row, .. } => (column, row),
    };
    if column < cols && row < rows {
        return Some(position);
    }
    if cols == 0 || rows == 0 || matches!(kind, MouseEventKind::Down(_) | MouseEventKind::Moved) {
        return None;
    }
    Some(crate::protocol::ClientMousePosition::Cell {
        column: column.min(cols - 1),
        row: row.min(rows - 1),
    })
}

pub(super) fn apply_client_terminal_input_events(
    runtime: &crate::terminal::TerminalRuntime,
    events: &[ClientPaneInputEvent],
    host_page_keys: bool,
    keyboard_forwarded: &mut bool,
) -> Result<(), String> {
    for event in events {
        if let ClientPaneInputEvent::Mouse {
            kind,
            position,
            modifiers,
            lines,
            ..
        } = event
        {
            let kind = kind.to_crossterm();
            let modifiers = KeyModifiers::from_bits_truncate(*modifiers);
            let Some(position) =
                fit_mouse_position_to_grid(kind, *position, runtime.current_size())
            else {
                continue;
            };
            let position = match &position {
                crate::protocol::ClientMousePosition::Cell { column, row } => {
                    crate::input::mouse::Position::Cell {
                        column: *column,
                        row: *row,
                    }
                }
                crate::protocol::ClientMousePosition::Pixels { x, y, column, row } => {
                    if runtime.sgr_pixel_mouse_enabled() {
                        crate::input::mouse::Position::Pixels { x: *x, y: *y }
                    } else {
                        crate::input::mouse::Position::Cell {
                            column: *column,
                            row: *row,
                        }
                    }
                }
            };
            let bytes = match kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let direction = if kind == MouseEventKind::ScrollUp {
                        AttachScrollDirection::Up
                    } else {
                        AttachScrollDirection::Down
                    };
                    apply_scroll(
                        runtime,
                        AttachScrollSource::Wheel,
                        direction,
                        (*lines).max(1),
                        position,
                        modifiers.bits(),
                    )?;
                    continue;
                }
                MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => runtime
                    .encode_mouse_wheel(kind, position, modifiers)
                    .unwrap_or_default(),
                MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                    runtime
                        .encode_mouse_button(kind, position, modifiers)
                        .unwrap_or_default()
                }
                MouseEventKind::Moved => runtime
                    .encode_mouse_motion(kind, position, modifiers)
                    .unwrap_or_default(),
            };
            if !bytes.is_empty() {
                if kind != MouseEventKind::Moved {
                    runtime.scroll_reset();
                }
                runtime
                    .try_send_bytes(Bytes::from(bytes))
                    .map_err(|err| format!("targeted pane mouse input failed: {err}"))?;
            }
            continue;
        }

        match event.to_raw_input_event() {
            crate::raw_input::RawInputEvent::Key(key) => {
                let key_event = key.as_key_event();
                if host_page_keys
                    && matches!(key_event.code, KeyCode::PageUp | KeyCode::PageDown)
                    && key_event.modifiers.is_empty()
                    && runtime.plain_page_keys_use_host_scrollback() == Some(true)
                {
                    match key_event.kind {
                        KeyEventKind::Release => continue,
                        KeyEventKind::Press | KeyEventKind::Repeat => {
                            let lines = runtime.current_size().0.max(1) as usize;
                            if key_event.code == KeyCode::PageUp {
                                runtime.scroll_up(lines);
                            } else {
                                runtime.scroll_down(lines);
                            }
                            continue;
                        }
                    }
                }

                runtime.scroll_reset();
                let bytes = runtime.encode_terminal_key(key);
                if !bytes.is_empty() {
                    runtime
                        .try_send_bytes(Bytes::from(bytes))
                        .map_err(|err| format!("targeted pane key input failed: {err}"))?;
                    *keyboard_forwarded |= key_event.kind != KeyEventKind::Release;
                }
            }
            crate::raw_input::RawInputEvent::Text(text) => {
                runtime.scroll_reset();
                runtime
                    .try_send_bytes(Bytes::copy_from_slice(text.as_str().as_bytes()))
                    .map_err(|err| format!("targeted pane text input failed: {err}"))?;
                *keyboard_forwarded |= !text.as_str().is_empty();
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                runtime.scroll_reset();
                runtime
                    .try_send_paste(text)
                    .map_err(|err| format!("targeted pane paste failed: {err}"))?;
            }
            crate::raw_input::RawInputEvent::Mouse(_)
            | crate::raw_input::RawInputEvent::OuterFocusGained
            | crate::raw_input::RawInputEvent::OuterFocusLost
            | crate::raw_input::RawInputEvent::HostDefaultColor { .. }
            | crate::raw_input::RawInputEvent::HostPaletteColors { .. }
            | crate::raw_input::RawInputEvent::HostColorSchemeChanged(_)
            | crate::raw_input::RawInputEvent::HostCellSizeReport { .. }
            | crate::raw_input::RawInputEvent::Unsupported => {
                return Err("non-pane input reached targeted pane input".to_owned());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_attach_stale_geometry_falls_back_to_the_canonical_cell() {
        let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"");
        let position = crate::protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };

        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (20, 5),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                position,
                Some(crate::protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            Some(crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (20, 5),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                true,
                false,
                crate::protocol::ClientMousePosition::Pixels {
                    x: 120,
                    y: 80,
                    column: 12,
                    row: 4,
                },
                Some(crate::protocol::ClientMouseGeometry {
                    cols: 20,
                    rows: 5,
                    width_px: 200,
                    height_px: 100,
                }),
            ),
            None
        );
        assert_eq!(
            terminal_attach_mouse_position(
                &runtime,
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                false,
                false,
                crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                None,
            ),
            Some(crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 })
        );
    }

    #[test]
    fn ineligible_shell_pixel_mouse_uses_its_canonical_cell_position() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, false, (5, 20), Some((200, 100)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                ..
            }]
        ));
    }

    #[test]
    fn eligible_shell_pixel_mouse_remains_exact() {
        let position = crate::protocol::ClientMousePosition::Pixels {
            x: 121,
            y: 81,
            column: 12,
            row: 4,
        };
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position,
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, true, (5, 20), Some((200, 100)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: current,
                ..
            }] if *current == position
        ));
    }

    #[test]
    fn stale_shell_pixel_geometry_downgrades_to_its_canonical_cell() {
        let mut events = vec![ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 121,
                y: 81,
                column: 12,
                row: 4,
            },
            geometry: Some(crate::protocol::ClientMouseGeometry {
                cols: 20,
                rows: 5,
                width_px: 200,
                height_px: 100,
            }),
            modifiers: 0,
            lines: 1,
        }];

        downgrade_ineligible_pixel_mouse(&mut events, true, (6, 20), Some((200, 120)));

        assert!(matches!(
            events.as_slice(),
            [ClientPaneInputEvent::Mouse {
                position: crate::protocol::ClientMousePosition::Cell { column: 12, row: 4 },
                geometry: None,
                ..
            }]
        ));
    }

    #[test]
    fn mouse_outside_the_pty_grid_drops_presses_and_clamps_releases() {
        use crate::protocol::ClientMousePosition::{Cell, Pixels};
        let grid = (17, 67);
        let inside = Cell {
            column: 66,
            row: 16,
        };
        assert_eq!(
            fit_mouse_position_to_grid(
                MouseEventKind::Down(crossterm::event::MouseButton::Left),
                inside,
                grid
            ),
            Some(inside)
        );
        let padding = Pixels {
            x: 900,
            y: 500,
            column: 90,
            row: 30,
        };
        for kind in [
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            assert_eq!(fit_mouse_position_to_grid(kind, padding, grid), None);
        }
        for kind in [
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            MouseEventKind::ScrollUp,
        ] {
            assert_eq!(
                fit_mouse_position_to_grid(kind, padding, grid),
                Some(Cell {
                    column: 66,
                    row: 16
                })
            );
        }
        assert_eq!(
            fit_mouse_position_to_grid(
                MouseEventKind::ScrollDown,
                Cell { column: 3, row: 40 },
                grid
            ),
            Some(Cell { column: 3, row: 16 })
        );
        assert_eq!(
            fit_mouse_position_to_grid(MouseEventKind::ScrollDown, inside, (0, 0)),
            None
        );
    }

    #[tokio::test]
    async fn a_click_in_pane_padding_never_reaches_the_child() {
        // SGR mouse reporting, so every forwarded press or release is visible.
        let (runtime, mut input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                20,
                5,
                0,
                b"\x1b[?1000h\x1b[?1006h",
                8,
            );
        let mouse = |kind, column, row| ClientPaneInputEvent::Mouse {
            kind,
            position: crate::protocol::ClientMousePosition::Cell { column, row },
            geometry: None,
            modifiers: 0,
            lines: 1,
        };
        let left = crate::protocol::ClientMouseButton::Left;

        apply_client_pane_input_events(
            &runtime,
            &[mouse(crate::protocol::ClientMouseKind::Down(left), 30, 2)],
        )
        .expect("padding press");
        assert!(
            input.try_recv().is_err(),
            "a press in the padding is dropped"
        );

        apply_client_pane_input_events(
            &runtime,
            &[
                mouse(crate::protocol::ClientMouseKind::Down(left), 4, 2),
                mouse(crate::protocol::ClientMouseKind::Up(left), 30, 2),
            ],
        )
        .expect("press inside, release in the padding");
        assert_eq!(
            input.try_recv().expect("press inside the grid"),
            Bytes::from_static(b"\x1b[<0;5;3M")
        );
        assert_eq!(
            input
                .try_recv()
                .expect("release clamped to the last column"),
            Bytes::from_static(b"\x1b[<0;20;3m")
        );
    }
}
