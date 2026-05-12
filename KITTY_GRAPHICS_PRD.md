# Kitty graphics support PRD

## Goal

Herdr should let pane programs use the Kitty graphics protocol without raw passthrough. Each pane remains a normal libghostty-backed terminal. Herdr reads pane-local image state from libghostty, translates it into the composed outer terminal layout, and paints images in the host terminal.

## Non-goals for the first phase

The first phase does not make the headless server/client protocol image-aware. It does not preserve images across detach/reattach. It does not guarantee scrollback image history. It does not support file, temporary-file, or shared-memory image media. It does not support arbitrary non-pane UI images.

## Task 1: local pane graphics

Visible panes in the attached local UI can display Kitty graphics. A hidden workspace or tab may accumulate libghostty image state while hidden; when it becomes visible, herdr repaints the visible placements for that pane. Pane splits, tab bar, sidebar, borders, zoom, and resizing are handled by mapping pane-local viewport coordinates into the outer terminal coordinates.

Acceptance criteria:

- Each visible pane in the active workspace/tab can receive direct Kitty image commands and render the image in its pane.
- Hidden panes do not paint into the host terminal while hidden.
- Switching tabs/workspaces clears stale host-terminal images and paints the newly visible pane placements.
- Resizing, splitting, closing, or zooming panes clears stale placements and repaints from libghostty state.
- Herdr does not forward raw pane Kitty escape sequences to the outer terminal.
- Image storage remains pane-local in libghostty and host-terminal image IDs are namespaced by herdr.
- File, temp-file, and shared-memory media stay disabled.

Implementation shape:

- Bind libghostty Kitty graphics and system APIs used by the embedder.
- Install a process-global PNG decoder if PNG payload support is enabled.
- Enable a bounded Kitty image storage limit on pane terminals.
- Pass real host cell pixel geometry to libghostty resize and PTY resize.
- After the ratatui text frame is drawn, query visible pane placements, clip to each pane inner rect, and emit host Kitty graphics commands for those placements.
- Clear herdr-owned host image IDs before repainting to avoid stale images after layout changes.

## Task 2: multiplexer-complete graphics

Detached clients, reattached clients, multiple clients, scrollback image history, and client-specific host-terminal caches are supported. This is the hard tmux-class part because herdr must synchronize image state across clients that have independent terminal image caches.

Acceptance criteria:

- A newly attached client receives enough image metadata/data to reconstruct the current visible placements without waiting for the pane app to redraw.
- Multiple attached clients can render the same pane image state with separate host-terminal image ID namespaces.
- Scrolling a pane moves, clips, hides, and reveals images consistently with text scrollback.
- Hidden tabs/workspaces retain state without leaking host-terminal images into visible clients.
- Server/client protocol has explicit image messages or an equivalent terminal-graphics stream with size limits and cache lifecycle.
- Images are deleted when panes close, workspaces disappear, clients detach, or host-terminal geometry invalidates placements.

Implementation shape:

- Extend the headless protocol beyond `FrameData` cells to carry image descriptors and payloads or pre-encoded Kitty graphics commands.
- Track per-client image caches and ID allocation.
- Add viewport-aware image rendering for scrollback, including virtual/placeholder placements if required.
- Add backpressure and size limits for large images.
- Add tests around tab switches, detach/reattach, multiple clients, and scrollback.
