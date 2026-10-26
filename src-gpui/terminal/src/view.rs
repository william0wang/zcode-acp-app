//! Main terminal view component for GPUI.
//!
//! This module provides [`TerminalView`], the primary component for embedding terminals
//! in GPUI applications. It manages:
//!
//! - **I/O Streams**: Accepts arbitrary [`Read`]/[`Write`]
//!   streams, allowing integration with any PTY implementation
//! - **Event Handling**: Keyboard and mouse input, with configurable callbacks
//! - **Rendering**: Efficient canvas-based rendering via [`TerminalRenderer`]
//! - **Configuration**: Font, colors, dimensions, and padding via [`TerminalConfig`]
//!
//! # Architecture
//!
//! The terminal uses a push-based async I/O architecture:
//!
//! 1. A background thread reads bytes from the PTY stdout in 4KB chunks
//! 2. Bytes are sent through a [flume](https://docs.rs/flume) channel to an async task
//! 3. The async task processes bytes through the VTE parser and calls `cx.notify()`
//! 4. GPUI repaints the terminal with the updated grid
//!
//! This approach ensures the terminal only wakes when data arrives, avoiding polling.
//!
//! # Thread Safety
//!
//! - [`TerminalView`] itself is not `Send` (it contains GPUI handles)
//! - The stdin writer is wrapped in `Arc<parking_lot::Mutex<>>` for thread-safe writes
//! - Callbacks ([`ResizeCallback`], [`KeyHandler`]) must be `Send + Sync`
//!
//! # Example
//!
//! ```ignore
//! use gpui::{Context, Edges, px};
//! use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
//!
//! // In a GPUI window context:
//! let terminal = cx.new(|cx| {
//!     TerminalView::new(pty_writer, pty_reader, TerminalConfig::default(), cx)
//!         .with_resize_callback(move |cols, rows| {
//!             // Notify PTY of new dimensions
//!         })
//!         .with_exit_callback(|_, cx| {
//!             cx.quit();
//!         })
//! });
//!
//! // Focus the terminal to receive keyboard input
//! terminal.read(cx).focus_handle().focus(window);
//! ```

use crate::colors::ColorPalette;
use crate::event::{GpuiEventProxy, TerminalEvent};
use crate::input::keystroke_to_bytes;
use crate::mouse::{pixel_to_cell, selection_type_from_clicks, url_span_at, Selection};
use crate::render::TerminalRenderer;
use crate::terminal::TerminalState;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::term::cell::Flags as CellFlags;
use gpui::{Edges, prelude::FluentBuilder as _, *};
use std::io::{Read, Write};
use std::ops::Range;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;

/// Configuration for terminal creation and runtime updates.
///
/// This struct defines the terminal's appearance and behavior, including
/// grid dimensions, font settings, scrollback buffer, and color scheme.
///
/// # Default Values
///
/// | Field | Default |
/// |-------|---------|
/// | `cols` | 80 |
/// | `rows` | 24 |
/// | `font_family` | "monospace" |
/// | `font_size` | 14px |
/// | `scrollback` | 10000 |
/// | `line_height_multiplier` | 1.0 |
/// | `padding` | 0px all sides |
/// | `colors` | Default palette |
///
/// # Example
///
/// ```ignore
/// use gpui::{Edges, px};
/// use gpui_terminal::{ColorPalette, TerminalConfig};
///
/// let config = TerminalConfig {
///     cols: 120,
///     rows: 40,
///     font_family: "JetBrains Mono".into(),
///     font_size: px(13.0),
///     scrollback: 50000,
///     line_height_multiplier: 1.0,
///     padding: Edges::all(px(10.0)),
///     colors: ColorPalette::builder()
///         .background(0x1a, 0x1a, 0x1a)
///         .foreground(0xe0, 0xe0, 0xe0)
///         .build(),
/// };
/// ```
///
/// # Runtime Updates
///
/// Configuration can be updated at runtime via [`TerminalView::update_config`].
/// This is useful for implementing features like dynamic font sizing:
///
/// ```ignore
/// terminal.update(cx, |terminal, cx| {
///     let mut config = terminal.config().clone();
///     config.font_size += px(1.0);
///     terminal.update_config(config, cx);
/// });
/// ```
#[derive(Clone, Debug)]
pub struct TerminalConfig {
    /// Number of columns (character width) in the terminal
    pub cols: usize,

    /// Number of rows (lines) in the terminal
    pub rows: usize,

    /// Font family name (e.g., "Fira Code", "JetBrains Mono")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Maximum number of scrollback lines to keep in history
    pub scrollback: usize,

    /// Multiplier for line height to accommodate tall glyphs (e.g., nerd fonts)
    /// Default is 1.0 (no extra height)
    pub line_height_multiplier: f32,

    /// Padding around the terminal content (top, right, bottom, left)
    /// The padding area renders with the terminal's background color
    pub padding: Edges<Pixels>,

    /// Color palette for terminal colors (16 ANSI colors, 256 extended colors,
    /// foreground, background, and cursor colors)
    pub colors: ColorPalette,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "monospace".into(),
            font_size: px(14.0),
            scrollback: 10000,
            line_height_multiplier: 1.0,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
        }
    }
}

/// Callback type for PTY resize notifications.
///
/// This callback is invoked when the terminal grid dimensions change,
/// typically due to window resizing. The callback receives the new
/// column and row counts.
///
/// # Arguments
///
/// * `cols` - New number of columns (characters wide)
/// * `rows` - New number of rows (lines tall)
///
/// # Thread Safety
///
/// This callback must be `Send + Sync` as it may be called from the render thread.
///
/// # Example
///
/// ```ignore
/// use portable_pty::PtySize;
///
/// let pty = Arc::new(Mutex::new(pty_master));
/// let pty_clone = pty.clone();
///
/// terminal.with_resize_callback(move |cols, rows| {
///     pty_clone.lock().resize(PtySize {
///         cols: cols as u16,
///         rows: rows as u16,
///         pixel_width: 0,
///         pixel_height: 0,
///     }).ok();
/// });
/// ```
pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;

/// Callback type for key event interception.
///
/// This callback is invoked before the terminal processes a key event,
/// allowing you to intercept and handle specific key combinations.
///
/// # Arguments
///
/// * `event` - The key down event from GPUI
///
/// # Returns
///
/// * `true` - Consume the event (terminal will not process it)
/// * `false` - Let the terminal handle the event normally
///
/// # Thread Safety
///
/// This callback must be `Send + Sync`.
///
/// # Example
///
/// ```ignore
/// terminal.with_key_handler(|event| {
///     let keystroke = &event.keystroke;
///
///     // Intercept Ctrl++ for font size increase
///     if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
///         // Handle font size increase
///         return true; // Consume the event
///     }
///
///     // Intercept Ctrl+- for font size decrease
///     if keystroke.modifiers.control && keystroke.key == "-" {
///         // Handle font size decrease
///         return true;
///     }
///
///     false // Let terminal handle all other keys
/// });
/// ```
pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;

/// Callback for terminal bell events.
///
/// This callback is invoked when the terminal bell is triggered (BEL character,
/// ASCII 0x07), allowing you to play a sound or show a visual indicator.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_bell_callback(|window, cx| {
///     // Option 1: Visual bell (flash the window or show an indicator)
///     // Option 2: Play a sound
///     // Option 3: Notify the user via system notification
/// });
/// ```
pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// Callback for terminal title changes.
///
/// This callback is invoked when the terminal title changes via escape sequences
/// (OSC 0, OSC 2), allowing you to update the window or tab title.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `title` - The new title string
///
/// # Example
///
/// ```ignore
/// terminal.with_title_callback(|window, cx, title| {
///     // Update the window title
///     // Or update a tab label in a tabbed interface
///     println!("Terminal title changed to: {}", title);
/// });
/// ```
pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for clipboard store requests.
///
/// This callback is invoked when the terminal wants to store data to the clipboard
/// via OSC 52 escape sequence. Applications like tmux and vim can use this to
/// copy text to the system clipboard.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `text` - The text to store in the clipboard
///
/// # Example
///
/// ```ignore
/// use gpui_terminal::Clipboard;
///
/// terminal.with_clipboard_store_callback(|window, cx, text| {
///     if let Ok(mut clipboard) = Clipboard::new() {
///         clipboard.copy(text).ok();
///     }
/// });
/// ```
pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for terminal exit events.
///
/// This callback is invoked when the terminal process exits (e.g., shell exits,
/// process terminates). This is detected when the PTY reader reaches EOF.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_exit_callback(|window, cx| {
///     // Option 1: Quit the application
///     cx.quit();
///
///     // Option 2: Close this terminal tab/pane
///     // terminal_manager.close_terminal(terminal_id);
///
///     // Option 3: Show an exit message
///     // show_notification("Terminal exited");
/// });
/// ```
pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// The main terminal view component for GPUI applications.
///
/// `TerminalView` is a GPUI entity that implements the [`Render`] trait,
/// providing a complete terminal emulator that can be embedded in any GPUI application.
///
/// # Responsibilities
///
/// - **Terminal State**: Manages the grid, cursor, and colors via [`TerminalState`]
/// - **I/O Streams**: Reads from PTY stdout and writes to PTY stdin
/// - **Event Handling**: Processes keyboard, mouse, and resize events
/// - **Rendering**: Paints text, backgrounds, and cursor via [`TerminalRenderer`]
/// - **Callbacks**: Dispatches events to user-provided callbacks
///
/// # Creating a Terminal
///
/// Use [`TerminalView::new`] within a GPUI entity context:
///
/// ```ignore
/// let terminal = cx.new(|cx| {
///     TerminalView::new(writer, reader, config, cx)
///         .with_resize_callback(resize_callback)
///         .with_exit_callback(|_, cx| cx.quit())
/// });
/// ```
///
/// # Focus
///
/// The terminal must be focused to receive keyboard input:
///
/// ```ignore
/// terminal.read(cx).focus_handle().focus(window);
/// ```
///
/// # Callbacks
///
/// Configure behavior through builder methods:
///
/// - [`with_resize_callback`](Self::with_resize_callback) - PTY size changes
/// - [`with_exit_callback`](Self::with_exit_callback) - Process exit
/// - [`with_key_handler`](Self::with_key_handler) - Key event interception
/// - [`with_bell_callback`](Self::with_bell_callback) - Terminal bell
/// - [`with_title_callback`](Self::with_title_callback) - Title changes
/// - [`with_clipboard_store_callback`](Self::with_clipboard_store_callback) - Clipboard writes
///
/// # Thread Safety
///
/// `TerminalView` is not `Send` as it contains GPUI handles. The stdin writer
/// is internally wrapped in `Arc<parking_lot::Mutex<>>` for safe concurrent access.
pub struct TerminalView {
    /// The terminal state managing the grid and VTE parser
    state: TerminalState,

    /// The renderer for drawing terminal content
    renderer: TerminalRenderer,

    /// Focus handle for keyboard event handling
    focus_handle: FocusHandle,

    /// Writer for sending input to the terminal process
    stdin_writer: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,

    /// Receiver for terminal events from the event proxy
    event_rx: mpsc::Receiver<TerminalEvent>,

    /// Configuration used to create this terminal
    config: TerminalConfig,

    /// Async task that reads bytes and notifies the view (push-based)
    #[allow(dead_code)]
    _reader_task: Task<()>,

    /// Callback to notify the PTY about size changes
    resize_callback: Option<Arc<ResizeCallback>>,

    /// Optional callback to intercept key events before terminal processing
    key_handler: Option<Arc<KeyHandler>>,

    /// Callback for terminal bell events
    bell_callback: Option<BellCallback>,

    /// Callback for terminal title changes
    title_callback: Option<TitleCallback>,

    /// Callback for clipboard store requests
    clipboard_store_callback: Option<ClipboardStoreCallback>,

    /// Callback for terminal exit events
    exit_callback: Option<ExitCallback>,

    /// The last painted canvas bounds, written by the render's paint pass —
    /// the mouse handlers' pixel→cell math needs the grid origin, and
    /// handlers have no other way to reach the element's bounds.
    view_bounds: Arc<parking_lot::Mutex<Option<Bounds<Pixels>>>>,

    /// The active text selection (buffer coordinates: lines may reach into
    /// scrollback, exactly the coordinates the grid indexes with).
    selection: Option<Selection>,

    /// True between the selection's mouse-down and its mouse-up.
    selecting: bool,

    /// The selection's fixed side — the (possibly word/line-expanded) span
    /// the mouse-down anchored; the drag moves only the other end.
    select_anchor: Option<(AlacPoint, AlacPoint)>,

    /// The URL under the pointer, if any (buffer line + column span).
    hover_link: Option<HoverLink>,

    /// A link press in flight: opens on mouse-up if the pointer never left
    /// the span (a drag off the link cancels the open).
    pressed_link: Option<String>,
}

/// A URL the pointer sits on: where it paints (buffer line + column span)
/// and what a click opens.
#[derive(Debug, Clone, PartialEq)]
struct HoverLink {
    line: i32,
    cols: Range<usize>,
    url: String,
}

impl TerminalView {
    /// Create a new terminal with provided I/O streams.
    ///
    /// This method initializes a new terminal emulator with the given stdin writer
    /// and stdout reader. It spawns a background task to read from stdout and
    /// process incoming bytes through the VTE parser.
    ///
    /// # Arguments
    ///
    /// * `stdin_writer` - Writer for sending input bytes to the terminal process
    /// * `stdout_reader` - Reader for receiving output bytes from the terminal process
    /// * `config` - Terminal configuration (dimensions, font, etc.)
    /// * `cx` - GPUI context for this view
    ///
    /// # Returns
    ///
    /// A new `TerminalView` instance ready to be rendered.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // In a GPUI window context:
    /// let terminal = cx.new(|cx| {
    ///     TerminalView::new(stdin_writer, stdout_reader, TerminalConfig::default(), cx)
    /// });
    /// ```
    pub fn new<W, R>(
        stdin_writer: W,
        stdout_reader: R,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        // Create event channel for terminal events
        let (event_tx, event_rx) = mpsc::channel();

        // Clone event_tx for the reader task to send Exit event when PTY closes
        let exit_event_tx = event_tx.clone();

        // Create event proxy for alacritty
        let event_proxy = GpuiEventProxy::new(event_tx);

        // Create terminal state
        let state = TerminalState::with_scrollback(
            config.cols,
            config.rows,
            config.scrollback,
            event_proxy,
        );

        // Create renderer with font settings and color palette
        let renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
        );

        // Create focus handle
        let focus_handle = cx.focus_handle();

        // Wrap stdin writer in Arc<Mutex> for thread-safe access
        let stdin_writer = Arc::new(parking_lot::Mutex::new(
            Box::new(stdin_writer) as Box<dyn Write + Send>
        ));

        // Create async channel for bytes (push-based notification)
        // Using flume instead of smol::channel because flume is executor-agnostic
        // and properly wakes GPUI's async executor when data arrives
        let (bytes_tx, bytes_rx) = flume::unbounded::<Vec<u8>>();

        // Spawn background thread to read from stdout
        // This thread sends bytes through the async channel
        thread::spawn(move || {
            Self::read_stdout_blocking(stdout_reader, bytes_tx);
        });

        // Spawn async task that awaits on the channel and notifies the view
        // This is push-based: the task blocks until bytes arrive, then immediately notifies
        let reader_task = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                // Wait for bytes from the background reader (blocks until data arrives)
                match bytes_rx.recv_async().await {
                    Ok(bytes) => {
                        // Process bytes and notify the view
                        let result = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                            view.state.process_bytes(&bytes);
                            cx.notify();
                        });
                        if result.is_err() {
                            // View was dropped, exit
                            break;
                        }
                    }
                    Err(_) => {
                        // Channel closed - PTY has finished, send Exit event
                        let _ = exit_event_tx.send(TerminalEvent::Exit);
                        // Notify view to process the Exit event
                        let _ = this.update(cx, |_view, cx: &mut Context<Self>| {
                            cx.notify();
                        });
                        break;
                    }
                }
            }
        });

        Self {
            state,
            renderer,
            focus_handle,
            stdin_writer,
            event_rx,
            config,
            _reader_task: reader_task,
            resize_callback: None,
            key_handler: None,
            bell_callback: None,
            title_callback: None,
            clipboard_store_callback: None,
            exit_callback: None,
            view_bounds: Arc::new(parking_lot::Mutex::new(None)),
            selection: None,
            selecting: false,
            select_anchor: None,
            hover_link: None,
            pressed_link: None,
        }
    }

    /// Set a callback to be invoked when the terminal is resized.
    ///
    /// This callback should resize the underlying PTY to match the new dimensions.
    /// The callback receives (cols, rows) as arguments.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with (cols, rows) on resize
    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Set a callback to intercept key events before terminal processing.
    ///
    /// The callback receives the key event and should return `true` to consume
    /// the event (prevent the terminal from processing it), or `false` to allow
    /// normal terminal processing.
    ///
    /// # Arguments
    ///
    /// * `handler` - A function that receives key events and returns whether to consume them
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_key_handler(|event| {
    ///     // Handle Ctrl++ to increase font size
    ///     if event.keystroke.modifiers.control && event.keystroke.key == "+" {
    ///         // Handle the event
    ///         return true; // Consume the event
    ///     }
    ///     false // Let terminal handle it
    /// })
    /// ```
    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    /// Set a callback to be invoked when the terminal bell is triggered.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to play a sound or show a visual indicator.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the bell is triggered
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_bell_callback(|window, cx| {
    ///     // Play a sound or flash the screen
    /// })
    /// ```
    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal title changes.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the new title string.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the new title
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_title_callback(|window, cx, title| {
    ///     // Update window title or tab title
    /// })
    /// ```
    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal wants to store data to the clipboard.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the text to store. This is typically triggered by OSC 52 escape sequences.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the text to store
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_clipboard_store_callback(|window, cx, text| {
    ///     // Store text to system clipboard
    /// })
    /// ```
    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal process exits.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to close the terminal view or show an exit message.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the process exits
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_exit_callback(|window, cx| {
    ///     // Close the terminal tab or show exit message
    /// })
    /// ```
    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// Background thread that reads from stdout.
    ///
    /// This function runs in a background thread, continuously reading bytes
    /// from the stdout reader and sending them through the async channel.
    /// The async channel allows the main async task to be woken up immediately
    /// when data arrives (push-based).
    fn read_stdout_blocking<R: Read + Send + 'static>(
        mut stdout_reader: R,
        bytes_tx: flume::Sender<Vec<u8>>,
    ) {
        let mut buffer = [0u8; 4096];

        loop {
            match stdout_reader.read(&mut buffer) {
                Ok(0) => {
                    // EOF - channel will be dropped, signaling completion
                    break;
                }
                Ok(n) => {
                    // Send bytes to the async task
                    let bytes = buffer[..n].to_vec();
                    if bytes_tx.send(bytes).is_err() {
                        break; // Channel closed
                    }
                }
                Err(_) => {
                    // Read error
                    break;
                }
            }
        }
    }

    /// Handle keyboard input events.
    ///
    /// Converts GPUI keystrokes to terminal escape sequences and writes them
    /// to the stdin writer. If a key handler is set and returns true, the event
    /// is consumed and not sent to the terminal. Paste pulls the host
    /// clipboard through gpui (the vendored crate dropped upstream's arboard
    /// dependency); every consumed key stops propagation so the host's
    /// action bindings (an app-wide Esc, for one) never double-fire.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Check if key handler wants to consume this event
        if let Some(ref handler) = self.key_handler
            && handler(event)
        {
            cx.stop_propagation();
            return; // Event consumed by handler
        }

        // Paste: platform+v on macOS, ctrl+shift+v elsewhere. Newlines in
        // clipboard text become CR — Enter is 0x0D on a PTY.
        let modifiers = event.keystroke.modifiers;
        let paste = event.keystroke.key == "v"
            && if cfg!(target_os = "macos") {
                modifiers.platform && !modifiers.control
            } else {
                modifiers.control && modifiers.shift
            };
        if paste {
            if let Some(item) = cx.read_from_clipboard()
                && let Some(text) = item.text()
            {
                let text = text.replace('\n', "\r");
                let mut writer = self.stdin_writer.lock();
                let _ = writer.write_all(text.as_bytes());
                let _ = writer.flush();
            }
            cx.stop_propagation();
            return;
        }

        // Copy: platform+C on macOS, ctrl+shift+C elsewhere. A selection
        // copies and swallows the key; with nothing selected the shell
        // keeps its ^C (the terminal convention).
        let copy = event.keystroke.key == "c"
            && if cfg!(target_os = "macos") {
                modifiers.platform && !modifiers.control
            } else {
                modifiers.control && modifiers.shift
            };
        if copy {
            let text = self.selection_text();
            if !text.is_empty() {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                cx.stop_propagation();
                return;
            }
        }

        if let Some(bytes) = keystroke_to_bytes(&event.keystroke, self.state.mode()) {
            // Typing invalidates the selection, as in every terminal.
            self.selection = None;
            let mut writer = self.stdin_writer.lock();
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
            cx.stop_propagation();
        }
    }

    /// Handle mouse down events: focus, start a selection (or arm a link
    /// press — links win over selection), and report to the shell when it
    /// asked for mouse tracking (TODO upstream; SGR helpers exist in
    /// `mouse`).
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Request focus when clicking the terminal
        window.focus(&self.focus_handle, cx);

        if event.button != MouseButton::Left {
            cx.notify();
            return;
        }
        if let Some(link) = self.link_at(event.position) {
            self.pressed_link = Some(link.url);
            self.selection = None;
            self.selecting = false;
        } else {
            self.pressed_link = None;
            if let Some(cell) = self.cell_at(event.position) {
                let ty = selection_type_from_clicks(event.click_count);
                let (start, end) = match ty {
                    crate::mouse::SelectionType::Simple => (cell, cell),
                    _ => self.expand_selection(cell, ty),
                };
                self.select_anchor = Some((start, end));
                self.selection = Some(Selection::new(start, end, ty));
                self.selecting = true;
            } else {
                self.selection = None;
                self.selecting = false;
                self.select_anchor = None;
            }
        }
        cx.notify();
    }

    /// Handle mouse up events: end the drag, open an armed link if the
    /// pointer never left it, and collapse an empty selection (a plain
    /// click is not a selection).
    fn on_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selecting = false;
        if let Some(url) = self.pressed_link.take() {
            if self.hover_link.as_ref().is_some_and(|l| l.url == url) {
                cx.open_url(&url);
            }
        }
        let collapsed = self.selection.as_ref().is_some_and(|sel| {
            sel.start == sel.end
                && sel.selection_type == crate::mouse::SelectionType::Simple
        });
        if collapsed {
            self.selection = None;
        }
        cx.notify();
    }

    /// Handle mouse move events: extend the selection while dragging, and
    /// keep the hovered URL (pointer cursor + underline) in step.
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        if self.selecting {
            if let (Some(cell), Some((a0, a1))) =
                (self.cell_at(event.position), self.select_anchor)
            {
                if let Some(sel) = self.selection.as_ref() {
                    // The anchor side stays; only the dragged end moves.
                    // Word/line drags snap the moving end to its boundary
                    // and take the hull of the two spans.
                    let (start, end) = match sel.selection_type {
                        crate::mouse::SelectionType::Simple => {
                            if cell < a0 { (cell, a0) } else { (a0, cell) }
                        }
                        ty => {
                            let (b0, b1) = self.expand_selection(cell, ty);
                            (a0.min(b0), a1.max(b1))
                        }
                    };
                    self.selection = Some(Selection::new(start, end, sel.selection_type));
                    changed = true;
                }
            }
        }
        let link = self.link_at(event.position);
        if link != self.hover_link {
            self.hover_link = link;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Handle scroll events.
    ///
    /// Wheel scrollback: translate the delta to lines and move alacritty's
    /// display offset (the grid's own history; alternate-screen apps like
    /// htop ignore it, matching every real terminal).
    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use alacritty_terminal::grid::Scroll;

        let line_height = self.renderer.cell_height;
        let lines = match event.delta {
            ScrollDelta::Pixels(pt) => (pt.y / line_height).round(),
            ScrollDelta::Lines(l) => l.y,
        };
        if lines != 0.0 {
            // Positive delta is wheel-up: into the history.
            self.state
                .with_term_mut(|term| term.scroll_display(Scroll::Delta(lines as i32)));
            cx.notify();
        }
    }

    /// Process pending terminal events.
    ///
    /// This method drains all available events from the event receiver
    /// and handles them appropriately. Note: bytes are processed in the
    /// async reader task, not here.
    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Process terminal events (from alacritty event proxy)
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalEvent::Wakeup => {
                    // Terminal has new content - already handled by async task
                }
                TerminalEvent::Bell => {
                    if let Some(ref callback) = self.bell_callback {
                        callback(window, cx);
                    }
                }
                TerminalEvent::Title(title) => {
                    if let Some(ref callback) = self.title_callback {
                        callback(window, cx, &title);
                    }
                }
                TerminalEvent::ClipboardStore(text) => {
                    if let Some(ref callback) = self.clipboard_store_callback {
                        callback(window, cx, &text);
                    }
                }
                TerminalEvent::ClipboardLoad => {
                    // Terminal wants to load data from clipboard
                    // TODO: Implement clipboard integration
                }
                TerminalEvent::Exit => {
                    if let Some(ref callback) = self.exit_callback {
                        callback(window, cx);
                    }
                }
            }
        }
    }

    /// Get the current terminal dimensions.
    ///
    /// # Returns
    ///
    /// A tuple of (columns, rows).
    pub fn dimensions(&self) -> (usize, usize) {
        // The live grid, not TerminalState's cols/rows fields: the render
        // path resizes the term directly through `term_arc` (bypassing
        // `state.resize`), so those fields hold the spawn defaults forever.
        use alacritty_terminal::grid::Dimensions;
        self.state
            .with_term(|term| (term.columns(), term.screen_lines()))
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method should be called when the terminal view size changes.
    /// It updates the internal grid and notifies the terminal process of the new size.
    ///
    /// # Arguments
    ///
    /// * `cols` - New number of columns
    /// * `rows` - New number of rows
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }

    /// Get the current terminal configuration.
    ///
    /// # Returns
    ///
    /// A reference to the current configuration.
    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    // -- selection & links ---------------------------------------------------

    /// A window point → grid cell in BUFFER coordinates (the viewport row
    /// shifted by the scrollback the frame renders at, matching the paint
    /// loop's own indexing). None before the first paint leaves no bounds.
    fn cell_at(&self, position: Point<Pixels>) -> Option<AlacPoint> {
        let guard = self.view_bounds.lock();
        let Some(bounds) = *guard else {
            return None;
        };
        let origin = point(
            bounds.origin.x + self.config.padding.left,
            bounds.origin.y + self.config.padding.top,
        );
        let cell =
            pixel_to_cell(position, origin, self.renderer.cell_width, self.renderer.cell_height);
        let display_offset = self.state.with_term(|t| t.renderable_content().display_offset) as i32;
        Some(AlacPoint::new(Line(cell.line.0 - display_offset), cell.column))
    }

    /// The URL at a window point, if the cell's line holds one over it.
    fn link_at(&self, position: Point<Pixels>) -> Option<HoverLink> {
        let cell = self.cell_at(position)?;
        let text = self.line_text(cell.line.0);
        let (span, url) = url_span_at(&text, cell.column.0)?;
        Some(HoverLink {
            line: cell.line.0,
            cols: span,
            url,
        })
    }

    /// One grid line's text. Column mapping is 1:1 with chars — wide-char
    /// spacers are skipped, which can skew spans on CJK-heavy lines; URLs
    /// are ASCII, so the mapping holds where it matters.
    fn line_text(&self, line: i32) -> String {
        use alacritty_terminal::grid::Dimensions;
        self.state.with_term(|term| {
            let grid = term.grid();
            let line = line.clamp(grid.topmost_line().0, grid.bottommost_line().0);
            let mut text = String::new();
            for col in 0..grid.columns() {
                let cell = &grid[AlacPoint::new(Line(line), Column(col))];
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
            text
        })
    }

    /// Expand a point to its word (the non-blank run) or its whole line —
    /// double- and triple-click selections.
    fn expand_selection(
        &self,
        point: AlacPoint,
        ty: crate::mouse::SelectionType,
    ) -> (AlacPoint, AlacPoint) {
        use alacritty_terminal::grid::Dimensions;
        self.state.with_term(|term| {
            let grid = term.grid();
            let blank =
                |p: AlacPoint| matches!(grid[p].c, ' ' | '\0');
            match ty {
                crate::mouse::SelectionType::Line => {
                    let last = grid.columns() - 1;
                    (
                        AlacPoint::new(point.line, Column(0)),
                        AlacPoint::new(point.line, Column(last)),
                    )
                }
                _ => {
                    let mut start = point.column.0;
                    while start > 0 && !blank(AlacPoint::new(point.line, Column(start - 1))) {
                        start -= 1;
                    }
                    let mut end = point.column.0;
                    while end + 1 < grid.columns()
                        && !blank(AlacPoint::new(point.line, Column(end + 1)))
                    {
                        end += 1;
                    }
                    (
                        AlacPoint::new(point.line, Column(start)),
                        AlacPoint::new(point.line, Column(end)),
                    )
                }
            }
        })
    }

    /// The selection's text (trailing blanks trimmed per row, rows joined
    /// with newlines) — ⌘C's payload; empty with no selection.
    fn selection_text(&self) -> String {
        use alacritty_terminal::grid::Dimensions;
        let Some(sel) = self.selection.as_ref() else {
            return String::new();
        };
        let (start, end) = if sel.start < sel.end {
            (sel.start, sel.end)
        } else {
            (sel.end, sel.start)
        };
        self.state.with_term(|term| {
            let grid = term.grid();
            let cols = grid.columns();
            let first = start.line.0.max(grid.topmost_line().0);
            let mut rows: Vec<String> = Vec::new();
            for line in first..=end.line.0 {
                let from = if line == start.line.0 { start.column.0 } else { 0 };
                let to = if line == end.line.0 { end.column.0 } else { cols - 1 };
                let mut row = String::new();
                for col in from..=to.min(cols - 1) {
                    let cell = &grid[AlacPoint::new(Line(line), Column(col))];
                    if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    row.push(if cell.c == '\0' { ' ' } else { cell.c });
                }
                rows.push(row.trim_end().to_string());
            }
            rows.join("\n")
        })
    }

    /// Get the focus handle for this terminal view.
    ///
    /// # Returns
    ///
    /// A reference to the focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Update the terminal configuration.
    ///
    /// This method updates the terminal's configuration, including font settings,
    /// padding, and color palette. Changes take effect on the next render.
    ///
    /// # Arguments
    ///
    /// * `config` - The new configuration to apply
    /// * `cx` - The context for triggering a repaint
    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        // Update renderer with new font settings and palette
        self.renderer.font_family = config.font_family.clone();
        self.renderer.font_size = config.font_size;
        self.renderer.line_height_multiplier = config.line_height_multiplier;
        self.renderer.palette = config.colors.clone();

        // Store the new config
        self.config = config;

        // Trigger a repaint - cell dimensions will be recalculated via measure_cell()
        cx.notify();
    }

    /// Calculate terminal dimensions from pixel bounds and cell size.
    ///
    /// Helper method to determine how many columns and rows fit in the given bounds.
    #[allow(dead_code)]
    fn calculate_dimensions(&self, bounds: Bounds<Pixels>) -> (usize, usize) {
        let width_f32: f32 = bounds.size.width.into();
        let height_f32: f32 = bounds.size.height.into();
        let cell_width_f32: f32 = self.renderer.cell_width.into();
        let cell_height_f32: f32 = self.renderer.cell_height.into();

        let cols = ((width_f32 / cell_width_f32) as usize).max(1);
        let rows = ((height_f32 / cell_height_f32) as usize).max(1);
        (cols, rows)
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process any pending events
        self.process_events(window, cx);

        // Measure HERE as well: the paint below measures only its own
        // clone, and the mouse handlers map pixels through self.renderer's
        // cells — without this they would use the font-agnostic defaults.
        self.renderer.measure_cell(window);

        // Get terminal state and renderer for rendering
        let state_arc = self.state.term_arc();
        let renderer = self.renderer.clone();
        let resize_callback = self.resize_callback.clone();
        let padding = self.config.padding;
        let selection = self.selection.clone();
        let hover = self
            .hover_link
            .as_ref()
            .map(|link| (link.line, link.cols.clone()));
        let bounds_sink = self.view_bounds.clone();
        let hover_active = self.hover_link.is_some();

        div()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .track_focus(&self.focus_handle)
            .map(|this| {
                if hover_active {
                    this.cursor_pointer()
                } else {
                    this
                }
            })
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |bounds, _, window, cx| {
                        use alacritty_terminal::grid::Dimensions;

                        // The mouse handlers' pixel→cell math reads these.
                        *bounds_sink.lock() = Some(bounds);

                        // Measure actual cell dimensions from the font
                        let mut measured_renderer = renderer.clone();
                        measured_renderer.measure_cell(window);

                        // Calculate available space after padding
                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = measured_renderer.cell_width.into();
                        let cell_height_f32: f32 = measured_renderer.cell_height.into();

                        let cols = ((available_width / cell_width_f32) as usize).max(1);
                        let rows = ((available_height / cell_height_f32) as usize).max(1);

                        // Helper struct implementing Dimensions for resize
                        struct TermSize {
                            cols: usize,
                            rows: usize,
                        }
                        impl Dimensions for TermSize {
                            fn total_lines(&self) -> usize {
                                self.rows
                            }
                            fn screen_lines(&self) -> usize {
                                self.rows
                            }
                            fn columns(&self) -> usize {
                                self.cols
                            }
                            fn last_column(&self) -> alacritty_terminal::index::Column {
                                alacritty_terminal::index::Column(self.cols.saturating_sub(1))
                            }
                            fn bottommost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(self.rows as i32 - 1)
                            }
                            fn topmost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(0)
                            }
                        }

                        // Resize terminal if dimensions changed
                        let mut term = state_arc.lock();
                        let current_cols = term.columns();
                        let current_rows = term.screen_lines();
                        if cols != current_cols || rows != current_rows {
                            // Notify the PTY about the resize
                            if let Some(ref callback) = resize_callback {
                                callback(cols, rows);
                            }
                            term.resize(TermSize { cols, rows });
                        }

                        // Paint the terminal with measured dimensions
                        measured_renderer.paint(
                            bounds,
                            padding,
                            &term,
                            selection.as_ref(),
                            hover.as_ref(),
                            window,
                            cx,
                        );
                    },
                )
                .size_full(),
            )
    }
}

// Tests are omitted due to macro expansion issues with the test attribute
// in this configuration. Integration tests can be added separately.
