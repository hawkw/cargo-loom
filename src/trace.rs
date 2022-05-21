use std::{
    fmt,
    sync::atomic::{AtomicU8, Ordering},
};

use color_eyre::Result;
use heck::TitleCase;
pub use owo_colors::{style, OwoColorize, Style};
use tracing::{field::Field, Event, Level, Subscriber};
use tracing_subscriber::{
    field::Visit,
    fmt::{format::Writer, FmtContext, FormatEvent, FormatFields, FormattedFields},
    registry::LookupSpan,
};

#[derive(Debug, clap::Parser)]
pub struct TraceSettings {
    /// Controls when colored output is used.
    ///
    /// Valid values:
    ///
    /// •  auto (default): Automatically detect if color support is available on the terminal.
    ///
    /// •  always: Always display colors.
    ///
    /// •  never: Never display colors.
    #[clap(long, env = "CARGO_TERM_COLORS", default_value = "auto", arg_enum)]
    color: ColorMode,

    /// The output format for trace messages and diagnostics.
    ///
    /// Valid values:
    ///
    /// •  human (default): Display in a human-readable text format.
    ///
    /// •  json: Emit JSON-formatted logs.
    #[clap(long, default_value = "human", arg_enum)]
    message_format: MessageFormat,

    /// A filter string controlling what traces are enabled.
    #[clap(long = "trace", default_value = "cargo=info,warn", env = "CARGO_LOG")]
    filter: tracing_subscriber::EnvFilter,
}

impl TraceSettings {
    pub fn message_format(&self) -> MessageFormat {
        self.message_format
    }

    pub fn try_init(&mut self) -> Result<()> {
        let filter = std::mem::take(&mut self.filter);
        self.try_init_with(filter)
    }

    pub fn try_init_with(&self, filter: tracing_subscriber::EnvFilter) -> Result<()> {
        use tracing_subscriber::prelude::*;
        self.color.set_global();
        self.message_format.set_global();
        let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
        let fmt = match self.message_format {
            MessageFormat::Human => fmt
                .event_format(CargoFormatter {
                    styles: Styles::new(self.color),
                })
                .boxed(),
            MessageFormat::Json => fmt.json().boxed(),
        };

        tracing_subscriber::registry()
            .with(fmt)
            .with(filter)
            .try_init()?;
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ArgEnum)]
#[repr(u8)]
pub enum ColorMode {
    Auto = 0,
    Always = 1,
    Never = 2,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, clap::ArgEnum)]
#[repr(u8)]
pub enum MessageFormat {
    Human = 0,
    Json = 1,
}

// === impl ColorMode ===

static GLOBAL_COLOR_MODE: AtomicU8 = AtomicU8::new(0);

impl fmt::Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl ColorMode {
    pub fn current() -> Self {
        match GLOBAL_COLOR_MODE.load(Ordering::Acquire) {
            x if x == Self::Always as u8 => Self::Always,
            x if x == Self::Never as u8 => Self::Never,
            _x => {
                debug_assert_eq!(_x, Self::Auto as u8, "weird color mode, what the heck?");
                Self::Auto
            }
        }
    }

    pub fn if_color(self, style: owo_colors::Style) -> owo_colors::Style {
        if self.should_color_stderr() {
            style
        } else {
            owo_colors::style()
        }
    }

    fn set_global(self) {
        GLOBAL_COLOR_MODE
            .compare_exchange(0, self as u8, Ordering::AcqRel, Ordering::Acquire)
            .expect("global color mode already set");
    }

    fn as_str(&self) -> &'static str {
        match self {
            ColorMode::Auto => "auto",
            ColorMode::Always => "always",
            ColorMode::Never => "never",
        }
    }

    // pub fn should_color_stdout(self) -> bool {
    //     match self {
    //         ColorMode::Auto => atty::is(atty::Stream::Stdout),
    //         ColorMode::Always => true,
    //         ColorMode::Never => false,
    //     }
    // }

    pub fn should_color_stderr(self) -> bool {
        match self {
            ColorMode::Auto => atty::is(atty::Stream::Stderr),
            ColorMode::Always => true,
            ColorMode::Never => false,
        }
    }
}

impl Default for ColorMode {
    fn default() -> Self {
        Self::current()
    }
}

// === impl MessageFormat ===

impl MessageFormat {
    pub fn is_json(self) -> bool {
        self == MessageFormat::Json
    }

    pub fn current() -> Self {
        match GLOBAL_COLOR_MODE.load(Ordering::Acquire) {
            x if x == Self::Human as u8 => Self::Human,
            x if x == Self::Json as u8 => Self::Json,
            _x => {
                #[cfg(debug_assertions)]
                panic!("weird message format {}", _x);

                #[cfg(not(debug_assertions))]
                Self::Human
            }
        }
    }

    fn set_global(self) {
        GLOBAL_COLOR_MODE
            .compare_exchange(0, self as u8, Ordering::AcqRel, Ordering::Acquire)
            .expect("global color mode already set");
    }
}

impl Default for MessageFormat {
    fn default() -> Self {
        Self::current()
    }
}

#[derive(Debug)]
struct CargoFormatter {
    styles: Styles,
}

struct Visitor<'styles, 'writer> {
    level: Level,
    writer: Writer<'writer>,
    is_empty: bool,
    styles: &'styles Styles,
    did_cargo_format: bool,
    from_escargot: bool,
}

#[derive(Debug)]
struct Styles {
    error: Style,
    warn: Style,
    info: Style,
    debug: Style,
    trace: Style,
    pipes: Style,
    bold: Style,
}

struct Prefixed<T> {
    prefix: &'static str,
    val: T,
}

impl<S, N> FormatEvent<S, N> for CargoFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        const LOG_TARGET: &str = "log.target";

        let metadata = event.metadata();
        let level = metadata.level();
        let from_escargot = metadata.fields().field(LOG_TARGET).is_some();

        let include_spans = {
            let mut visitor = self.visitor(*level, writer.by_ref(), from_escargot);
            event.record(&mut visitor);
            !visitor.did_cargo_format && ctx.lookup_current().is_some()
        };

        writer.write_char('\n')?;

        if include_spans {
            writeln!(
                writer,
                "   {} {}{}",
                "-->".style(self.styles.pipes),
                metadata.file().unwrap_or_else(|| metadata.target()),
                DisplayOpt(metadata.line().map(Prefixed::prefix(":"))),
            )?;
            ctx.visit_spans(|span| {
                let exts = span.extensions();
                let fields = exts
                    .get::<FormattedFields<N>>()
                    .map(|f| f.fields.as_str())
                    .unwrap_or("");
                writeln!(
                    writer,
                    "    {} {}{}{}",
                    "|".style(self.styles.pipes),
                    span.name().style(self.styles.bold),
                    if fields.is_empty() { "" } else { ": " },
                    fields
                )
            })?;

            writer.write_char('\n')?;
        }

        Ok(())
    }
}

impl CargoFormatter {
    fn visitor<'styles, 'writer>(
        &'styles self,
        level: Level,
        writer: Writer<'writer>,
        from_escargot: bool,
    ) -> Visitor<'styles, 'writer> {
        Visitor {
            level,
            writer,
            is_empty: true,
            styles: &self.styles,
            did_cargo_format: false,
            from_escargot,
        }
    }
}

// === impl Visitor ===

impl<'styles, 'writer> Visitor<'styles, 'writer> {
    const MESSAGE: &'static str = "message";
    const INDENT: usize = 12;
}

impl<'styles, 'writer> Visit for Visitor<'styles, 'writer> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();

        if name.starts_with("log.") {
            return;
        }

        // If we're writing the first field of the event, either emit cargo
        // formatting, or a level header.
        if self.is_empty {
            // If the level is `INFO` and it has a message that's
            // shaped like a cargo log tag, emit the cargo tag followed by the
            // rest of the message.
            if self.level == Level::INFO && name == Self::MESSAGE {
                let message = format!("{:?}", value);
                if let Some((tag, message)) = message.as_str().split_once(' ') {
                    if tag.len() <= Self::INDENT {
                        let tag = tag.to_title_case();
                        let style = match self.level {
                            Level::DEBUG => self.styles.debug,
                            _ => self.styles.info,
                        };

                        let _ = write!(
                            self.writer,
                            "{:>indent$} ",
                            tag.style(style),
                            indent = Self::INDENT
                        );

                        let _ = self.writer.write_str(message);
                        self.is_empty = false;
                        self.did_cargo_format = true;
                        return;
                    }
                }
            }

            // Handle forwarded cargo output from escargot.
            if name == Self::MESSAGE && self.from_escargot {
                let message = format!("{:?}", value);

                let message = if let Some(message) = message.strip_prefix("error") {
                    let _ = write!(self.writer, "{}", "error".style(self.styles.error));
                    message
                } else if let Some(message) = message.strip_prefix("warning") {
                    let _ = write!(self.writer, "{}", "warning".style(self.styles.warn));
                    message
                } else {
                    message.as_str()
                };

                let mut lines = message.lines();
                if let Some(first_line) = lines.next() {
                    let _ = writeln!(self.writer, "{}", first_line.style(self.styles.bold));
                    for line in lines {
                        let _ = self.writer.write_str(line);
                        let _ = self.writer.write_char('\n');
                    }
                }

                self.is_empty = false;
                return;
            }

            // Otherwise, emit a level tag.
            let _ = match self.level {
                Level::ERROR => write!(
                    self.writer,
                    "{}{} ",
                    "error".style(self.styles.error),
                    ":".style(self.styles.bold)
                ),
                Level::WARN => write!(
                    self.writer,
                    "{}{} ",
                    "warning".style(self.styles.warn),
                    ":".style(self.styles.bold),
                ),
                Level::INFO => write!(
                    self.writer,
                    "{}{} ",
                    "info".style(self.styles.info),
                    ":".style(self.styles.bold)
                ),
                Level::DEBUG => write!(
                    self.writer,
                    "{}{} ",
                    "debug".style(self.styles.debug),
                    ":".style(self.styles.bold)
                ),
                Level::TRACE => write!(
                    self.writer,
                    "{}{} ",
                    "trace".style(self.styles.trace),
                    ":".style(self.styles.bold)
                ),
            };
        } else {
            // If this is *not* the first field of the event, prefix it with a
            // comma for the preceding field, instead of a cargo tag or level tag.
            let _ = self.writer.write_str(", ");
        }

        if name == Self::MESSAGE {
            let _ = write!(self.writer, "{:?}", value.style(self.styles.bold));
        } else {
            let _ = write!(
                self.writer,
                "{}{} {:?}",
                name.style(self.styles.bold),
                ":".style(self.styles.bold),
                value
            );
        }

        self.is_empty = false;
    }
}

// === impl Styles ===

impl Styles {
    fn new(colors: ColorMode) -> Self {
        Self {
            error: colors.if_color(style().red().bold()),
            warn: colors.if_color(style().yellow().bold()),
            info: colors.if_color(style().green().bold()),
            debug: colors.if_color(style().blue().bold()),
            trace: colors.if_color(style().purple().bold()),
            bold: colors.if_color(style().bold()),
            pipes: colors.if_color(style().blue().bold()),
        }
    }
}

impl<T> Prefixed<T> {
    fn prefix(prefix: &'static str) -> impl Fn(T) -> Prefixed<T> {
        move |val| Prefixed { val, prefix }
    }
}

impl<T> fmt::Display for Prefixed<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.prefix, self.val)
    }
}

impl<T> fmt::Debug for Prefixed<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{:?}", self.prefix, self.val)
    }
}

struct DisplayOpt<T>(Option<T>);

impl<T> fmt::Display for DisplayOpt<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref val) = self.0 {
            fmt::Display::fmt(val, f)?;
        }

        Ok(())
    }
}
