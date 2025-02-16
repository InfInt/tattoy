//! An in-memory TTY renderer. It takes a stream of PTY output bytes and maintains the visual
//! appearance of a terminal without actually physically rendering it.

use snafu::ResultExt as _;
use termwiz::surface::Change as TermwizChange;
use termwiz::surface::Position as TermwizPosition;
use tracing::Instrument as _;

/// Wezterm's internal configuration
#[derive(Debug)]
struct WeztermConfig {
    /// The number of lines to store in the scrollback
    scrollback: usize,
}

impl wezterm_term::TerminalConfiguration for WeztermConfig {
    fn scrollback_size(&self) -> usize {
        self.scrollback
    }

    fn color_palette(&self) -> wezterm_term::color::ColorPalette {
        wezterm_term::color::ColorPalette::default()
    }
}

/// Config for creating a shadow terminal.
#[expect(
    clippy::exhaustive_structs,
    reason = "
        I just really like the ability to specify config in a struct. As if it were JSON.
        I know that means projects depending on this struct run the risk of unexpected
        breakage when I add a new field. But maybe we can manage those expectations by
        making sure that all example code is based off `ShadowTerminalConfig::default()`?
    "
)]
pub struct Config {
    /// Width of terminal
    pub width: u16,
    /// Height of terminal
    pub height: u16,
    /// Initial command for PTY, usually the user's `$SHELL`
    pub command: Vec<std::ffi::OsString>,
    /// The size of ther terminal's scrollback history.
    pub scrollback_size: usize,
    /// The number of lines that each scroll trigger moves.
    pub scrollback_step: usize,
}

impl Default for Config {
    #[inline]
    fn default() -> Self {
        Self {
            width: 100,
            height: 30,
            command: vec!["bash".into()],
            scrollback_size: 1000,
            scrollback_step: 5,
        }
    }
}

/// The scrollback is a history, albeit limited, of all the output whilst in REPL mode, aka the
/// "primary screen".
#[derive(Default)]
#[non_exhaustive]
pub struct Scrollback {
    /// The actual surface data, maybe of a cell per character.
    pub surface: termwiz::surface::Surface,
    /// The current position of the user's view on the scollback. Is 0 when not scrolling.
    pub position: usize,
}

/// Output data that can be output by the terminal.
#[non_exhaustive]
pub enum Surface {
    /// All of the output generated by the terminal whilst in REPL mode, aka, the "primary screen".
    Scrollback(Scrollback),
    /// The actual active view of the screen. It doesn't matter whether the terminal is scolling,
    /// or is in the "alternate screen", this is what you would see if you were using this
    /// terminal.
    Screen(termwiz::surface::Surface),
}

/// The kinds of surfaces that can be output.
#[derive(Debug)]
enum SurfaceKind {
    /// See [`Surface::Scrollback`]
    Scrollback,
    /// See [`Surface::Screen`]
    Screen,
}

impl Default for Surface {
    #[inline]
    fn default() -> Self {
        Self::Scrollback(Scrollback::default())
    }
}

/// The various inter-task/thread channels needed to run the shadow terminal and the PTY
/// simultaneously.
#[non_exhaustive]
pub struct Channels {
    /// Internal channel for control messages like shutdown and resize.
    pub control_tx: tokio::sync::broadcast::Sender<crate::Protocol>,
    /// The channel side that sends terminal output updates.
    pub output_tx: tokio::sync::mpsc::Sender<crate::pty::BytesFromPTY>,
    /// The channel side that receives terminal output updates.
    pub output_rx: tokio::sync::mpsc::Receiver<crate::pty::BytesFromPTY>,
    /// Sends complete snapshots of the current screen state.
    shadow_output: tokio::sync::mpsc::Sender<Surface>,
}

// TODO: Would it be useful to keep the PTY's task handle on here, and `await` it in the main loop,
// so that the PTY module always has time to do its shutdown?
//
/// This is the main Shadow Terminal struct that helps run everything is this crate.
///
/// Instantiating this struct will allow you to have steppable control over the shadow terminal. If you
/// want the shadow terminal to run unhindered, you can use `.run()`, though [`ActiveTerminal`] offers a
/// more convenient ready-made wrapper to interect with a running shadow terminal.
#[non_exhaustive]
pub struct ShadowTerminal {
    /// The Wezterm terminal that does most of the actual work of maintaining the terminal 🙇
    pub terminal: wezterm_term::Terminal,
    /// The shadow terminal's config
    pub config: Config,
    /// The various channels needed to run the shadow terminal and its PTY
    pub channels: Channels,
    /// Whether the terminal is in the so-called "alternative" screen or not
    pub is_alternative_screen: bool,
    /// The current position of the scollback buffer.
    scroll_position: usize,
}

impl ShadowTerminal {
    /// Create a new Shadow Terminal
    #[inline]
    pub fn new(config: Config, shadow_output: tokio::sync::mpsc::Sender<Surface>) -> Self {
        let (control_tx, _) = tokio::sync::broadcast::channel(64);
        let (output_tx, output_rx) = tokio::sync::mpsc::channel(1);

        tracing::debug!("Creating the in-memory Wezterm terminal");
        let terminal = wezterm_term::Terminal::new(
            Self::wezterm_size(config.width.into(), config.height.into()),
            std::sync::Arc::new(WeztermConfig {
                scrollback: config.scrollback_size,
            }),
            "Tattoy",
            "O_o",
            Box::<Vec<u8>>::default(),
        );

        Self {
            terminal,
            config,
            channels: Channels {
                control_tx,
                output_tx,
                output_rx,
                shadow_output,
            },
            is_alternative_screen: false,
            scroll_position: 0,
        }
    }

    /// Start the background PTY process.
    #[inline]
    pub fn start(
        &self,
        input_rx: tokio::sync::mpsc::Receiver<crate::pty::BytesFromSTDIN>,
    ) -> tokio::task::JoinHandle<Result<(), crate::errors::PTYError>> {
        let pty = crate::pty::PTY {
            command: self.config.command.clone(),
            width: self.config.width,
            height: self.config.height,
            control_tx: self.channels.control_tx.clone(),
            output_tx: self.channels.output_tx.clone(),
        };

        // I don't think the PTY should be run in a standard thread, because it's not actually CPU
        // intensive in terms of the current thread. It runs in an OS sub process, so in theory
        // shouldn't conflict with Tokio's IO-focussed scheduler?
        let current_span = tracing::Span::current();
        tokio::spawn(async move { pty.run(input_rx).instrument(current_span).await })
    }

    /// Start listening to a stream of PTY bytes and render them to a shadow Termwiz surface
    #[inline]
    pub async fn run(&mut self, input_rx: tokio::sync::mpsc::Receiver<crate::pty::BytesFromSTDIN>) {
        tracing::debug!("Starting Shadow Terminal loop...");

        let mut control_rx = self.channels.control_tx.subscribe();
        self.start(input_rx);

        tracing::debug!("Starting Shadow Terminal main loop");
        #[expect(
            clippy::integer_division_remainder_used,
            reason = "`tokio::select! generates this.`"
        )]
        loop {
            tokio::select! {
                bytes = self.channels.output_rx.recv() => self.send_output(bytes.as_ref()).await,
                Ok(message) = control_rx.recv() => {
                    self.handle_protocol_message(&message).await;
                    if matches!(message, crate::Protocol::End) {
                        break;
                    }
                }
                // TODO: I don't actually understand the conditions in which this is called.
                else => {
                    let result = self.kill();
                    if let Err(error) = result {
                        tracing::error!("{error:?}");
                    }
                    break;
                }
            }
        }

        tracing::debug!("Shadow Terminal loop finished");
    }

    // TODO:
    // The output of the PTY seems to be capped at 4095 bytes. Making the size of
    // [`crate::pty::BytesFromPTY`] bigger than that doesn't seem to make a difference. This means
    // that for large screen updates `self.build_current_surface()` can be called an unnecessary
    // number of times.
    //
    // Possible solutions:
    //   * Ideally get the PTY to send bigger payloads.
    //   * Only call `self.build_current_surface()` at a given frame rate, probably 60fps.
    //     This could be augmented with a check for the size so the payloads smaller than
    //     4095 get rendered immediately.
    //   * When receiving a payload of exactly 4095 bytes, wait a fixed amount of time for
    //     more payloads, because in most cases 4095 means that there wasn't enough room to
    //     fit everything in a single payload.
    //   * Make `self.build_current_surface()` able to detect new payloads as they happen
    //     so it can cancel itself and immediately start working on the new one.
    //
    /// Send the current state of the shadow terminal as a Termwiz surface to whoever is externally
    /// listening.
    async fn send_output(&mut self, maybe_bytes: Option<&crate::pty::BytesFromPTY>) {
        if let Some(bytes) = maybe_bytes {
            self.terminal.advance_bytes(bytes);
            tracing::trace!("Wezterm shadow terminal advanced {} bytes", bytes.len());
        }

        // TODO: consider adding this as a field on `Surface::Screen()`
        if self.terminal.is_alt_screen_active() != self.is_alternative_screen {
            self.is_alternative_screen = self.terminal.is_alt_screen_active();
            let result = self
                .channels
                .control_tx
                .send(crate::Protocol::IsAlternateScreen(
                    self.terminal.is_alt_screen_active(),
                ));
            if let Err(error) = result {
                tracing::error!("Sending IsAlternateScreen protocol message: {error:?}");
            }
        }

        // We _always_ send the screen, because a terminal _always_ displays _something_.
        let surface = self.build_current_surface(&SurfaceKind::Screen);
        let result = self
            .channels
            .shadow_output
            .send(Surface::Screen(surface))
            .await;
        if let Err(error) = result {
            tracing::error!("Sending shadow output screen: {error:?}");
        }

        // If we're not in the altrenate screen then the screen _and_ the scrollback has changed.
        // So we need to send the scrollback. At some point we need to figure out how to just send
        // the changes, rather than the whole thing!
        if !self.is_alternative_screen {
            let scroolback_surface = self.build_current_surface(&SurfaceKind::Scrollback);
            let scrollback = Scrollback {
                surface: scroolback_surface,
                position: self.scroll_position,
            };

            let scrollback_result = self
                .channels
                .shadow_output
                .send(Surface::Scrollback(scrollback))
                .await;
            if let Err(error) = scrollback_result {
                tracing::error!("Sending shadow output scrollback: {error:?}");
            }
        }
    }

    /// Broadcast the shutdown signal. This should exit both the underlying PTY process and the
    /// main `ShadowTerminal` loop.
    ///
    /// # Errors
    /// If the `End` messaage could not be sent.
    #[inline]
    pub fn kill(&self) -> Result<(), crate::errors::ShadowTerminalError> {
        tracing::debug!("`ShadowTerminal.kill()` called");

        self.channels
            .control_tx
            .send(crate::Protocol::End)
            .with_whatever_context(|err| {
                format!("Couldn't write bytes into PTY's STDIN: {err:?}")
            })?;

        Ok(())
    }

    /// Handle any messages from the internal control protocol
    async fn handle_protocol_message(&mut self, message: &crate::Protocol) {
        tracing::debug!("Shadow Terminal received protocol message: {message:?}");

        #[expect(clippy::wildcard_enum_match_arm, reason = "It's our internal protocol")]
        match message {
            crate::Protocol::Resize { width, height } => {
                self.terminal.resize(Self::wezterm_size(
                    usize::from(*width),
                    usize::from(*height),
                ));
            }
            crate::Protocol::Scroll(scroll) => {
                match scroll {
                    crate::Scroll::Up => {
                        let size = self.terminal.get_size();
                        let total_lines = self.terminal.screen().scrollback_rows() - size.rows;

                        self.scroll_position += self.config.scrollback_step;
                        self.scroll_position = self.scroll_position.min(total_lines);
                    }
                    crate::Scroll::Down => {
                        if self.scroll_position < self.config.scrollback_step {
                            self.scroll_position = 0;
                        } else {
                            self.scroll_position -= self.config.scrollback_step;
                        }
                    }
                    crate::Scroll::Cancel => {
                        self.scroll_position = 0;
                    }
                }

                self.send_output(None).await;
            }

            _ => (),
        };
    }

    // TODO:
    //   * Explore using this to improve performance:
    //     `self.terminal.screen().get_changed_stable_rows()
    /// Converts Wezterms's maintained virtual TTY into a compositable Termwiz surface
    fn build_current_surface(&mut self, kind: &SurfaceKind) -> termwiz::surface::Surface {
        tracing::trace!("Converting Wezterm terminal state to a `termwiz::surface::Surface`");

        let screen_size = self.terminal.get_size();
        let total_lines = self.terminal.screen().scrollback_rows();

        let size = match kind {
            SurfaceKind::Scrollback => Self::wezterm_size(screen_size.cols, total_lines),
            SurfaceKind::Screen => screen_size,
        };
        let mut surface = termwiz::surface::Surface::new(size.cols, size.rows);

        let range = match kind {
            SurfaceKind::Scrollback => 0..total_lines,
            SurfaceKind::Screen => {
                let bottom = if self.is_alternative_screen {
                    total_lines
                } else {
                    total_lines - self.scroll_position
                };

                let top = bottom - size.rows;
                top..bottom
            }
        };

        let mut screen = self
            .terminal
            .screen_mut()
            .lines_in_phys_range(range.clone());
        tracing::trace!(
            "Building Wezterm {kind:?} from lines: {range:?} ({})",
            screen.len()
        );
        for (y, line) in screen.iter_mut().enumerate() {
            for (x, cell) in line.cells_mut().iter().enumerate() {
                let attrs = cell.attrs();
                let cursor = TermwizChange::CursorPosition {
                    x: TermwizPosition::Absolute(x),
                    y: TermwizPosition::Absolute(y),
                };
                surface.add_change(cursor);

                // TODO: is there a more elegant way to copy over all the attributes?
                let attributes = vec![
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Foreground(
                        attrs.foreground(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Background(
                        attrs.background(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Intensity(
                        attrs.intensity(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Italic(
                        attrs.italic(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Underline(
                        attrs.underline(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Blink(attrs.blink())),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::Reverse(
                        attrs.reverse(),
                    )),
                    TermwizChange::Attribute(termwiz::cell::AttributeChange::StrikeThrough(
                        attrs.strikethrough(),
                    )),
                    cell.str().into(),
                ];
                surface.add_changes(attributes);
            }
        }

        let users_cursor = self.terminal.cursor_pos();
        let cursor = TermwizChange::CursorPosition {
            x: TermwizPosition::Absolute(users_cursor.x),
            #[expect(
                clippy::as_conversions,
                clippy::cast_sign_loss,
                clippy::cast_possible_truncation,
                reason = "We're well within the limits of usize"
            )]
            y: TermwizPosition::Absolute(users_cursor.y as usize),
        };
        surface.add_change(cursor);

        surface
    }

    /// Just a convenience wrapper around the native Wezterm type
    const fn wezterm_size(width: usize, height: usize) -> wezterm_term::TerminalSize {
        wezterm_term::TerminalSize {
            cols: width,
            rows: height,
            pixel_width: 0,
            pixel_height: 0,
            dpi: 0,
        }
    }

    /// Resize the underlying PTY. That's the only way to send the resquired OS `SIGWINCH`.
    ///
    /// # Errors
    /// If the `Protocol::Resize` message cannot be sent.
    #[inline]
    pub fn resize(
        &mut self,
        width: u16,
        height: u16,
    ) -> Result<(), tokio::sync::broadcast::error::SendError<crate::Protocol>> {
        self.channels
            .control_tx
            .send(crate::Protocol::Resize { width, height })?;
        self.terminal
            .resize(Self::wezterm_size(width.into(), height.into()));
        Ok(())
    }
}

impl Drop for ShadowTerminal {
    #[inline]
    fn drop(&mut self) {
        tracing::trace!("Running ShadowTerminal.drop()");
        let result = self.kill();
        if let Err(error) = result {
            tracing::error!("{error:?}");
        }
    }
}
