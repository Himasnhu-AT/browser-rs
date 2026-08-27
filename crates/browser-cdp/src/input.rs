//! `Input.dispatchMouseEvent`, `Input.dispatchKeyEvent` and
//! `Input.insertText` — the half of the live view that lets a human take over.
//!
//! [`CdpPage::screenshot`] sits here too, because
//! `crates/browser-server/API.md` §4 puts it in this module's block: it is the
//! still-frame counterpart of [`crate::screencast`], and like everything else
//! here it hands the caller plain data rather than a CDP type, so
//! `browser-server` never needs a `chromiumoxide` dependency of its own.
//!
//! # Why this exists
//!
//! The operator watches their agent drive a page, and at the point where a
//! login form appears they take the controls and type the password themselves.
//! Everything after that happens in the same browser context, so the agent
//! carries on already authenticated. That hand-over is three CDP commands, and
//! the details below are all that stand between them and "the click does
//! nothing".
//!
//! # Coordinates
//!
//! Page space, in CSS pixels, always. A screencast frame is device pixels and
//! the dashboard's `<img>` is neither, so *somebody* has to scale — and that
//! somebody is the server, which is the only layer holding both the frame's
//! [`crate::screencast::FrameMetadata`] and the rendered size. Nothing here
//! scales anything.
//!
//! # Keys
//!
//! A bare `keyDown` moves focus and fires `keydown` handlers but **inserts no
//! character**: the character comes from a `char` event's `text` field. That is
//! why [`KeyEvent::char`] exists and why [`CdpPage::insert_text`] — one command
//! for a whole string, and the only sane way to fill a password field — is
//! preferred for anything longer than a keystroke.
//!
//! [`key_definition`] is a US-layout table mapping a DOM `key` name onto the
//! `code` and `windowsVirtualKeyCode` Chrome wants alongside it. A page that
//! reads `event.key` works without it; a page that reads `event.keyCode`, and
//! plenty still do, does not.
//!
//! # What a synthetic keystroke cannot do
//!
//! A `keyDown` held with Ctrl, Alt or Meta carries no `text` — Ctrl+A is
//! "select all", not "select all *and* type an `a`" — but Chrome also does not
//! run the **editing command** behind that combination for a synthetic event.
//! So the page sees the keystroke and any handler of its own fires, while
//! select-all, copy and paste do not happen. Chrome exposes those through
//! `Input.dispatchKeyEvent`'s `commands` field, which this layer does not send:
//! deciding that Ctrl+A means `selectAll` is a keymap decision, and a keymap
//! belongs to whatever surface is translating a real human's keyboard.

use base64::Engine as _;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
// `Viewport` under its CDP name would collide with `crate::flags::Viewport`,
// which is the emulated window size rather than a screenshot's crop rectangle.
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, CaptureScreenshotParams, GetLayoutMetricsParams, Viewport as ClipRect,
};
use chromiumoxide::Page;

use crate::cdp::CdpPage;
use crate::error::{BrowserError, Result};

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

/// The modifier bit field CDP takes: `Alt=1`, `Ctrl=2`, `Meta=4`, `Shift=8`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Modifiers(u8);

impl Modifiers {
    /// No modifier held.
    pub const NONE: Self = Self(0);
    /// Alt / Option.
    pub const ALT: Self = Self(1);
    /// Control.
    pub const CTRL: Self = Self(2);
    /// Meta / Command / Super.
    pub const META: Self = Self(4);
    /// Shift.
    pub const SHIFT: Self = Self(8);

    /// The modifiers `bits` describes, with anything above bit 3 discarded.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & 0b1111)
    }

    /// The raw bit field.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every modifier in `other` is held.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether *any* modifier in `other` is held.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// `self` plus `other`.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The CDP field, omitted when nothing is held — CDP's own default is 0.
    fn as_cdp(self) -> Option<i64> {
        (self.0 != 0).then_some(i64::from(self.0))
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.with(other)
    }
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

/// Which mouse button an event is about.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Button {
    /// No button — a plain move, or a wheel.
    #[default]
    None,
    /// The primary button.
    Left,
    /// The middle button.
    Middle,
    /// The secondary button.
    Right,
    /// The "back" thumb button.
    Back,
    /// The "forward" thumb button.
    Forward,
}

impl Button {
    /// The CDP spelling.
    #[must_use]
    pub fn as_cdp(self) -> MouseButton {
        match self {
            Self::None => MouseButton::None,
            Self::Left => MouseButton::Left,
            Self::Middle => MouseButton::Middle,
            Self::Right => MouseButton::Right,
            Self::Back => MouseButton::Back,
            Self::Forward => MouseButton::Forward,
        }
    }

    /// This button's bit in CDP's `buttons` mask.
    #[must_use]
    pub const fn mask(self) -> i64 {
        match self {
            Self::None => 0,
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
            Self::Back => 8,
            Self::Forward => 16,
        }
    }

    /// The wire name the dashboard protocol uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

/// Which of the four `Input.dispatchMouseEvent` types to send.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum MouseEventKind {
    /// `mouseMoved`.
    #[default]
    Moved,
    /// `mousePressed`.
    Pressed,
    /// `mouseReleased`.
    Released,
    /// `mouseWheel`.
    Wheel,
}

impl MouseEventKind {
    /// The CDP spelling.
    #[must_use]
    pub fn as_cdp(self) -> DispatchMouseEventType {
        match self {
            Self::Moved => DispatchMouseEventType::MouseMoved,
            Self::Pressed => DispatchMouseEventType::MousePressed,
            Self::Released => DispatchMouseEventType::MouseReleased,
            Self::Wheel => DispatchMouseEventType::MouseWheel,
        }
    }
}

/// One mouse event, in **page space** (CSS pixels of the viewport).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseEvent {
    /// Which event to send.
    pub kind: MouseEventKind,
    /// X, in CSS pixels from the left of the viewport.
    pub x: f64,
    /// Y, in CSS pixels from the top of the viewport.
    pub y: f64,
    /// The button this event is about.
    pub button: Button,
    /// How many clicks this is part of — `2` is the second of a double-click.
    pub click_count: u32,
    /// Modifiers held.
    pub modifiers: Modifiers,
    /// Horizontal wheel delta, for [`MouseEventKind::Wheel`].
    pub delta_x: f64,
    /// Vertical wheel delta, for [`MouseEventKind::Wheel`].
    pub delta_y: f64,
}

impl MouseEvent {
    fn at(kind: MouseEventKind, x: f64, y: f64) -> Self {
        Self {
            kind,
            x,
            y,
            button: Button::None,
            click_count: 0,
            modifiers: Modifiers::NONE,
            delta_x: 0.0,
            delta_y: 0.0,
        }
    }

    /// A pointer move to `(x, y)` with nothing held.
    ///
    /// Worth sending before a press: some widgets refuse a click that arrived
    /// without the pointer ever having moved onto them.
    #[must_use]
    pub fn moved(x: f64, y: f64) -> Self {
        Self::at(MouseEventKind::Moved, x, y)
    }

    /// A button going down at `(x, y)`.
    #[must_use]
    pub fn pressed(x: f64, y: f64, button: Button, click_count: u32) -> Self {
        Self {
            button,
            click_count,
            ..Self::at(MouseEventKind::Pressed, x, y)
        }
    }

    /// A button coming back up at `(x, y)`.
    #[must_use]
    pub fn released(x: f64, y: f64, button: Button, click_count: u32) -> Self {
        Self {
            button,
            click_count,
            ..Self::at(MouseEventKind::Released, x, y)
        }
    }

    /// A wheel turn at `(x, y)`, in CSS pixels of scroll.
    #[must_use]
    pub fn wheel(x: f64, y: f64, delta_x: f64, delta_y: f64) -> Self {
        Self {
            delta_x,
            delta_y,
            ..Self::at(MouseEventKind::Wheel, x, y)
        }
    }

    /// The same event with `modifiers` held.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }

    /// CDP's `buttons` mask: which buttons are down *while* this event happens.
    ///
    /// A press is the button itself; a release is nothing, because by the time
    /// the event is delivered the button is up. A move carries the mask too, so
    /// that a move with a button set reads as a drag rather than a hover.
    #[must_use]
    pub const fn buttons(&self) -> i64 {
        match self.kind {
            MouseEventKind::Pressed | MouseEventKind::Moved => self.button.mask(),
            MouseEventKind::Released | MouseEventKind::Wheel => 0,
        }
    }
}

/// The `Input.dispatchMouseEvent` parameters for `event`.
///
/// Split out from [`CdpPage::dispatch_mouse`] so the wire shape can be pinned
/// by a test with no browser in it.
#[must_use]
pub fn mouse_params(event: &MouseEvent) -> DispatchMouseEventParams {
    let mut params = DispatchMouseEventParams::new(event.kind.as_cdp(), event.x, event.y);
    params.button = Some(event.button.as_cdp());
    params.buttons = Some(event.buttons());
    params.modifiers = event.modifiers.as_cdp();
    // `clickCount` is meaningless on a move and on a wheel, and Chrome's own
    // default of 0 is what a move should carry.
    if matches!(
        event.kind,
        MouseEventKind::Pressed | MouseEventKind::Released
    ) {
        params.click_count = Some(i64::from(event.click_count));
    }
    if event.kind == MouseEventKind::Wheel {
        params.delta_x = Some(event.delta_x);
        params.delta_y = Some(event.delta_y);
    }
    params
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

/// Which of the four `Input.dispatchKeyEvent` types to send.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum KeyEventKind {
    /// `keyDown` — fires `keydown`, and inserts text only if `text` is set.
    #[default]
    Down,
    /// `keyUp`.
    Up,
    /// `rawKeyDown` — `keydown` without any text processing at all.
    RawDown,
    /// `char` — the event that actually inserts a character.
    Char,
}

impl KeyEventKind {
    /// The CDP spelling.
    #[must_use]
    pub fn as_cdp(self) -> DispatchKeyEventType {
        match self {
            Self::Down => DispatchKeyEventType::KeyDown,
            Self::Up => DispatchKeyEventType::KeyUp,
            Self::RawDown => DispatchKeyEventType::RawKeyDown,
            Self::Char => DispatchKeyEventType::Char,
        }
    }

    /// Whether a `text` field belongs on this event.
    ///
    /// CDP: "Not needed for `keyUp` and `rawKeyDown` events."
    #[must_use]
    pub const fn carries_text(self) -> bool {
        matches!(self, Self::Down | Self::Char)
    }
}

/// One keyboard event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeyEvent {
    /// Which event to send.
    pub kind: KeyEventKind,
    /// DOM `event.key` — `"a"`, `"A"`, `"Enter"`, `"ArrowLeft"`.
    pub key: String,
    /// DOM `event.code` — the *physical* key, `"KeyA"`, `"Enter"`.
    pub code: String,
    /// The character this event inserts.
    ///
    /// The whole point of a `char` event; see the module docs.
    pub text: Option<String>,
    /// `event.keyCode` / `event.which`, which older page code reads.
    pub windows_virtual_key_code: Option<i64>,
    /// Modifiers held.
    pub modifiers: Modifiers,
}

impl KeyEvent {
    /// A key event of `kind` for the DOM key name `key`.
    ///
    /// `code`, `windowsVirtualKeyCode` and — for a text-carrying kind — `text`
    /// are filled in from [`key_definition`]. A key the table does not know
    /// still produces an event, carrying only `key`; Chrome accepts it and a
    /// page reading `event.key` still sees the right thing.
    ///
    /// A key that needs Shift on a US layout (`"A"`, `"?"`) gets
    /// [`Modifiers::SHIFT`] set, so a page listening for `keydown` sees the
    /// combination a human would have produced.
    #[must_use]
    pub fn new(kind: KeyEventKind, key: &str) -> Self {
        let Some(definition) = key_definition(key) else {
            return Self {
                kind,
                key: key.to_owned(),
                ..Self::default()
            };
        };
        Self {
            kind,
            key: definition.key,
            code: definition.code.to_owned(),
            text: definition.text.filter(|_| kind.carries_text()),
            windows_virtual_key_code: Some(definition.windows_virtual_key_code),
            modifiers: if definition.needs_shift {
                Modifiers::SHIFT
            } else {
                Modifiers::NONE
            },
        }
    }

    /// `keyDown` for `key`.
    #[must_use]
    pub fn down(key: &str) -> Self {
        Self::new(KeyEventKind::Down, key)
    }

    /// `keyUp` for `key`.
    #[must_use]
    pub fn up(key: &str) -> Self {
        Self::new(KeyEventKind::Up, key)
    }

    /// A `char` event that inserts `text`.
    ///
    /// This is the one that types. When `text` is a single key the table
    /// knows, `key`/`code`/`windowsVirtualKeyCode` come along too, so the page
    /// sees a plausible keystroke rather than a bare character.
    #[must_use]
    pub fn char(text: &str) -> Self {
        let mut event = Self::new(KeyEventKind::Char, text);
        event.text = Some(text.to_owned());
        event
    }

    /// Override `text`.
    #[must_use]
    pub fn with_text(mut self, text: &str) -> Self {
        self.text = Some(text.to_owned());
        self
    }

    /// Override `code` — use it when the caller already knows the physical key.
    #[must_use]
    pub fn with_code(mut self, code: &str) -> Self {
        self.code = code.to_owned();
        self
    }

    /// Override the modifiers, discarding any Shift [`KeyEvent::new`] inferred.
    #[must_use]
    pub fn with_modifiers(mut self, modifiers: Modifiers) -> Self {
        self.modifiers = modifiers;
        self
    }
}

/// The `Input.dispatchKeyEvent` parameters for `event`.
///
/// Split out from [`CdpPage::dispatch_key`] so the wire shape can be pinned by
/// a test with no browser in it.
///
/// One rule is applied here rather than left to the caller: a `keyDown` held
/// with Ctrl, Alt or Meta sends **no text**. Ctrl+A is "select all", not "select
/// all and also type an `a`", and a `text` field on that event would do both.
/// Shift is the exception — Shift+1 really does type `!`.
#[must_use]
pub fn key_params(event: &KeyEvent) -> DispatchKeyEventParams {
    let mut params = DispatchKeyEventParams::new(event.kind.as_cdp());
    if !event.key.is_empty() {
        params.key = Some(event.key.clone());
    }
    if !event.code.is_empty() {
        params.code = Some(event.code.clone());
    }
    let shortcut = event.kind == KeyEventKind::Down
        && event
            .modifiers
            .intersects(Modifiers::CTRL | Modifiers::ALT | Modifiers::META);
    if event.kind.carries_text() && !shortcut {
        params.text.clone_from(&event.text);
        // `unmodifiedText` is "what the keyboard would have generated with no
        // modifier pressed **except for shift**", so for everything this layer
        // sends the two are equal.
        params.unmodified_text.clone_from(&event.text);
    }
    params.windows_virtual_key_code = event.windows_virtual_key_code;
    params.modifiers = event.modifiers.as_cdp();
    params
}

/// What a DOM key name means on a US layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyDefinition {
    /// DOM `event.key`.
    pub key: String,
    /// DOM `event.code`, the physical key.
    pub code: &'static str,
    /// `event.keyCode` / `event.which`.
    pub windows_virtual_key_code: i64,
    /// The character a `char` event should insert, when there is one.
    pub text: Option<String>,
    /// Whether producing this key on a US layout needs Shift held.
    pub needs_shift: bool,
}

/// The named (non-printing) keys, as `(key, code, windowsVirtualKeyCode)`.
const NAMED_KEYS: &[(&str, &str, i64)] = &[
    ("Backspace", "Backspace", 8),
    ("Tab", "Tab", 9),
    ("Enter", "Enter", 13),
    ("Shift", "ShiftLeft", 16),
    ("Control", "ControlLeft", 17),
    ("Alt", "AltLeft", 18),
    ("Pause", "Pause", 19),
    ("CapsLock", "CapsLock", 20),
    ("Escape", "Escape", 27),
    ("PageUp", "PageUp", 33),
    ("PageDown", "PageDown", 34),
    ("End", "End", 35),
    ("Home", "Home", 36),
    ("ArrowLeft", "ArrowLeft", 37),
    ("ArrowUp", "ArrowUp", 38),
    ("ArrowRight", "ArrowRight", 39),
    ("ArrowDown", "ArrowDown", 40),
    ("Insert", "Insert", 45),
    ("Delete", "Delete", 46),
    ("Meta", "MetaLeft", 91),
    ("ContextMenu", "ContextMenu", 93),
    ("F1", "F1", 112),
    ("F2", "F2", 113),
    ("F3", "F3", 114),
    ("F4", "F4", 115),
    ("F5", "F5", 116),
    ("F6", "F6", 117),
    ("F7", "F7", 118),
    ("F8", "F8", 119),
    ("F9", "F9", 120),
    ("F10", "F10", 121),
    ("F11", "F11", 122),
    ("F12", "F12", 123),
];

/// The printable punctuation, as `(character, code, windowsVirtualKeyCode,
/// needs_shift)`.
///
/// Letters and digits are derived rather than listed: their virtual key code
/// *is* their uppercase ASCII value.
const PUNCTUATION: &[(char, &str, i64, bool)] = &[
    (';', "Semicolon", 186, false),
    (':', "Semicolon", 186, true),
    ('=', "Equal", 187, false),
    ('+', "Equal", 187, true),
    (',', "Comma", 188, false),
    ('<', "Comma", 188, true),
    ('-', "Minus", 189, false),
    ('_', "Minus", 189, true),
    ('.', "Period", 190, false),
    ('>', "Period", 190, true),
    ('/', "Slash", 191, false),
    ('?', "Slash", 191, true),
    ('`', "Backquote", 192, false),
    ('~', "Backquote", 192, true),
    ('[', "BracketLeft", 219, false),
    ('{', "BracketLeft", 219, true),
    ('\\', "Backslash", 220, false),
    ('|', "Backslash", 220, true),
    (']', "BracketRight", 221, false),
    ('}', "BracketRight", 221, true),
    ('\'', "Quote", 222, false),
    ('"', "Quote", 222, true),
    ('!', "Digit1", 49, true),
    ('@', "Digit2", 50, true),
    ('#', "Digit3", 51, true),
    ('$', "Digit4", 52, true),
    ('%', "Digit5", 53, true),
    ('^', "Digit6", 54, true),
    ('&', "Digit7", 55, true),
    ('*', "Digit8", 56, true),
    ('(', "Digit9", 57, true),
    (')', "Digit0", 48, true),
];

/// The DOM `code` for a letter or digit, e.g. `"KeyA"`, `"Digit7"`.
fn alphanumeric_code(character: char) -> Option<&'static str> {
    /// `"KeyA"` … `"KeyZ"`, indexed by `character as usize - 'A' as usize`.
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    /// `"Digit0"` … `"Digit9"`.
    const DIGITS: [&str; 10] = [
        "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
        "Digit9",
    ];
    if character.is_ascii_alphabetic() {
        return Some(LETTERS[character.to_ascii_uppercase() as usize - 'A' as usize]);
    }
    if character.is_ascii_digit() {
        return Some(DIGITS[character as usize - '0' as usize]);
    }
    None
}

/// What `key` is on a US layout, if this table knows.
///
/// `key` is a DOM `event.key` value: a single character for a printable key
/// (`"a"`, `"A"`, `"?"`, `" "`) and a name for anything else (`"Enter"`,
/// `"ArrowLeft"`, `"F5"`).
///
/// ```
/// use browser_cdp::input::key_definition;
///
/// let enter = key_definition("Enter").expect("a known key");
/// assert_eq!(enter.windows_virtual_key_code, 13);
/// // Chrome's `Enter` inserts a carriage return, not a newline.
/// assert_eq!(enter.text.as_deref(), Some("\r"));
///
/// let question = key_definition("?").expect("a known key");
/// assert_eq!(question.code, "Slash");
/// assert!(question.needs_shift);
/// ```
#[must_use]
pub fn key_definition(key: &str) -> Option<KeyDefinition> {
    if let Some(&(name, code, virtual_key)) = NAMED_KEYS.iter().find(|(name, _, _)| *name == key) {
        return Some(KeyDefinition {
            key: name.to_owned(),
            code,
            windows_virtual_key_code: virtual_key,
            // Chrome's own US layout gives these two a text, and a page that
            // handles Enter in a `<textarea>` depends on it.
            text: match name {
                "Enter" => Some("\r".to_owned()),
                "Tab" => Some("\t".to_owned()),
                _ => None,
            },
            needs_shift: false,
        });
    }

    let mut characters = key.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }

    if character == ' ' {
        return Some(KeyDefinition {
            key: " ".to_owned(),
            code: "Space",
            windows_virtual_key_code: 32,
            text: Some(" ".to_owned()),
            needs_shift: false,
        });
    }

    if let Some(code) = alphanumeric_code(character) {
        return Some(KeyDefinition {
            key: character.to_string(),
            code,
            // A letter's virtual key code is its *uppercase* ASCII value; a
            // digit's is its own. `to_ascii_uppercase` covers both.
            windows_virtual_key_code: i64::from(character.to_ascii_uppercase() as u8),
            text: Some(character.to_string()),
            needs_shift: character.is_ascii_uppercase(),
        });
    }

    PUNCTUATION
        .iter()
        .find(|(punctuation, _, _, _)| *punctuation == character)
        .map(
            |&(punctuation, code, virtual_key, needs_shift)| KeyDefinition {
                key: punctuation.to_string(),
                code,
                windows_virtual_key_code: virtual_key,
                text: Some(punctuation.to_string()),
                needs_shift,
            },
        )
}

// ---------------------------------------------------------------------------
// The page methods
// ---------------------------------------------------------------------------

impl CdpPage {
    /// Send one mouse event, in **page space**.
    ///
    /// A click is three of these — [`MouseEvent::moved`],
    /// [`MouseEvent::pressed`], [`MouseEvent::released`] — and the caller
    /// sends them, because a human taking over the controls presses and
    /// releases at moments this layer has no business inventing.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Input.dispatchMouseEvent` fails.
    pub async fn dispatch_mouse(&self, ev: MouseEvent) -> Result<()> {
        execute(self.raw(), mouse_params(&ev), "Input.dispatchMouseEvent").await
    }

    /// Send one keyboard event.
    ///
    /// Remember that only a [`KeyEventKind::Char`] event — or a `keyDown`
    /// carrying `text` — inserts a character. For a whole string, use
    /// [`CdpPage::insert_text`].
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Input.dispatchKeyEvent` fails.
    pub async fn dispatch_key(&self, ev: KeyEvent) -> Result<()> {
        execute(self.raw(), key_params(&ev), "Input.dispatchKeyEvent").await
    }

    /// Insert `text` at the focused element, as an IME would.
    ///
    /// One command for the whole string: no per-character round trip, and no
    /// key table to get wrong for text that is not on a US keyboard. It fires
    /// `input` but not `keydown`, so a page that only reacts to keystrokes
    /// needs [`CdpPage::dispatch_key`] instead.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Input.insertText` fails.
    pub async fn insert_text(&self, text: &str) -> Result<()> {
        execute(
            self.raw(),
            InsertTextParams::new(text.to_owned()),
            "Input.insertText",
        )
        .await
    }

    /// The page as PNG bytes, decoded.
    ///
    /// `full_page` captures the whole scrollable document rather than what is
    /// on screen. It does that with a `clip` covering
    /// `Page.getLayoutMetrics`'s `cssContentSize` — **not** by growing the
    /// viewport, which is slower and which the page can see through
    /// `window.innerHeight` and a resize event.
    ///
    /// Bytes rather than the base64 CDP returns, and no CDP type in the
    /// signature: `browser-server` has no `chromiumoxide` dependency and must
    /// not grow one, so the protocol stops here.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Cdp`] when `Page.getLayoutMetrics` or
    /// `Page.captureScreenshot` fails, or when what came back is not base64.
    pub async fn screenshot(&self, full_page: bool) -> Result<Vec<u8>> {
        let page = self.raw();
        let mut params = CaptureScreenshotParams {
            format: Some(CaptureScreenshotFormat::Png),
            ..CaptureScreenshotParams::default()
        };

        if full_page {
            let metrics = page
                .execute(GetLayoutMetricsParams::default())
                .await
                .map_err(|error| BrowserError::Cdp {
                    context: "Page.getLayoutMetrics".to_owned(),
                    message: error.to_string(),
                })?;
            let content = &metrics.result.css_content_size;
            params.clip = Some(ClipRect {
                x: content.x,
                y: content.y,
                width: content.width,
                height: content.height,
                // The device scale factor still applies on top of this; `1.0`
                // means "no extra zoom", not "one device pixel per CSS pixel".
                scale: 1.0,
            });
            // Older Chromes clip to the viewport whatever `clip` says. Newer
            // ones ignore the flag. Sending it is correct on both.
            params.capture_beyond_viewport = Some(true);
        }

        let returns = page
            .execute(params)
            .await
            .map_err(|error| BrowserError::Cdp {
                context: "Page.captureScreenshot".to_owned(),
                message: error.to_string(),
            })?;
        let base64: &str = returns.result.data.as_ref();
        base64::engine::general_purpose::STANDARD
            .decode(base64)
            .map_err(|error| BrowserError::Cdp {
                context: "Page.captureScreenshot".to_owned(),
                message: format!("the image was not base64: {error}"),
            })
    }
}

/// `page.execute`, with this crate's error on top.
///
/// A local twin of the one in [`crate::cdp`], which is private to that module.
async fn execute<T>(page: &Page, command: T, context: &str) -> Result<()>
where
    T: chromiumoxide::types::Command,
{
    page.execute(command)
        .await
        .map_err(|error| BrowserError::Cdp {
            context: context.to_owned(),
            message: error.to_string(),
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn json<T: serde::Serialize>(value: &T) -> serde_json::Value {
        serde_json::to_value(value).expect("the params serialise")
    }

    #[test]
    fn the_modifier_bits_are_cdps() {
        assert_eq!(Modifiers::ALT.bits(), 1);
        assert_eq!(Modifiers::CTRL.bits(), 2);
        assert_eq!(Modifiers::META.bits(), 4);
        assert_eq!(Modifiers::SHIFT.bits(), 8);

        let combined = Modifiers::CTRL | Modifiers::SHIFT;
        assert_eq!(combined.bits(), 10);
        assert!(combined.contains(Modifiers::CTRL));
        assert!(!combined.contains(Modifiers::ALT));
        assert_eq!(Modifiers::from_bits(0xff), Modifiers::from_bits(0b1111));
        assert_eq!(Modifiers::NONE.as_cdp(), None, "0 is CDP's own default");
    }

    #[test]
    fn a_press_carries_the_button_in_both_fields_and_a_release_carries_none() {
        assert_eq!(
            json(&mouse_params(&MouseEvent::pressed(
                12.5,
                34.0,
                Button::Left,
                1
            ))),
            serde_json::json!({
                "type": "mousePressed",
                "x": 12.5,
                "y": 34.0,
                "button": "left",
                "buttons": 1,
                "clickCount": 1,
            }),
        );
        assert_eq!(
            json(&mouse_params(&MouseEvent::released(
                12.5,
                34.0,
                Button::Left,
                1
            ))),
            serde_json::json!({
                "type": "mouseReleased",
                "x": 12.5,
                "y": 34.0,
                "button": "left",
                "buttons": 0,
                "clickCount": 1,
            }),
        );
    }

    #[test]
    fn a_plain_move_has_no_button_and_no_click_count() {
        assert_eq!(
            json(&mouse_params(&MouseEvent::moved(1.0, 2.0))),
            serde_json::json!({
                "type": "mouseMoved",
                "x": 1.0,
                "y": 2.0,
                "button": "none",
                "buttons": 0,
            }),
        );
    }

    #[test]
    fn a_move_with_a_button_held_reads_as_a_drag() {
        let drag = MouseEvent {
            button: Button::Left,
            ..MouseEvent::moved(5.0, 6.0)
        };
        assert_eq!(drag.buttons(), 1);
        assert_eq!(json(&mouse_params(&drag))["buttons"], 1);
    }

    #[test]
    fn the_right_and_middle_masks_are_not_the_button_order() {
        // CDP's `buttons` is the DOM mask: right is 2 and middle is 4, which
        // is *not* the order the `button` enum lists them in.
        assert_eq!(Button::Right.mask(), 2);
        assert_eq!(Button::Middle.mask(), 4);
        assert_eq!(
            json(&mouse_params(&MouseEvent::pressed(
                0.0,
                0.0,
                Button::Right,
                1
            )))["buttons"],
            2
        );
    }

    #[test]
    fn a_wheel_carries_its_deltas_and_nothing_else_does() {
        assert_eq!(
            json(&mouse_params(&MouseEvent::wheel(3.0, 4.0, 0.0, -120.0))),
            serde_json::json!({
                "type": "mouseWheel",
                "x": 3.0,
                "y": 4.0,
                "button": "none",
                "buttons": 0,
                "deltaX": 0.0,
                "deltaY": -120.0,
            }),
        );
        assert!(json(&mouse_params(&MouseEvent::moved(0.0, 0.0)))
            .get("deltaY")
            .is_none());
    }

    #[test]
    fn modifiers_ride_along_on_a_mouse_event() {
        let shifted = MouseEvent::pressed(0.0, 0.0, Button::Left, 1)
            .with_modifiers(Modifiers::SHIFT | Modifiers::META);
        assert_eq!(json(&mouse_params(&shifted))["modifiers"], 12);
    }

    #[test]
    fn a_char_event_carries_the_text_because_that_is_what_types() {
        let event = KeyEvent::char("a");
        assert_eq!(event.kind, KeyEventKind::Char);
        assert_eq!(
            json(&key_params(&event)),
            serde_json::json!({
                "type": "char",
                "key": "a",
                "code": "KeyA",
                "text": "a",
                "unmodifiedText": "a",
                "windowsVirtualKeyCode": 65,
            }),
        );
    }

    #[test]
    fn a_key_up_never_carries_text() {
        let params = json(&key_params(&KeyEvent::up("a")));
        assert_eq!(params["type"], "keyUp");
        assert_eq!(params["windowsVirtualKeyCode"], 65);
        assert!(params.get("text").is_none(), "CDP: not needed for keyUp");
        assert!(params.get("unmodifiedText").is_none());
    }

    #[test]
    fn enter_is_thirteen_and_inserts_a_carriage_return() {
        assert_eq!(
            json(&key_params(&KeyEvent::down("Enter"))),
            serde_json::json!({
                "type": "keyDown",
                "key": "Enter",
                "code": "Enter",
                "text": "\r",
                "unmodifiedText": "\r",
                "windowsVirtualKeyCode": 13,
            }),
        );
    }

    #[test]
    fn a_key_that_needs_shift_says_so() {
        let upper = KeyEvent::down("A");
        assert_eq!(upper.code, "KeyA");
        assert_eq!(upper.windows_virtual_key_code, Some(65));
        assert_eq!(upper.modifiers, Modifiers::SHIFT);
        assert_eq!(json(&key_params(&upper))["modifiers"], 8);

        let question = KeyEvent::char("?");
        assert_eq!(question.code, "Slash");
        assert_eq!(question.text.as_deref(), Some("?"));
        assert_eq!(question.modifiers, Modifiers::SHIFT);
    }

    #[test]
    fn an_unknown_key_still_produces_an_event_with_the_key_name() {
        let event = KeyEvent::down("BrightnessUp");
        assert_eq!(event.key, "BrightnessUp");
        assert_eq!(event.code, "");
        assert_eq!(event.windows_virtual_key_code, None);

        let params = json(&key_params(&event));
        assert_eq!(params["key"], "BrightnessUp");
        assert!(params.get("code").is_none(), "an empty code is not sent");
        assert!(params.get("windowsVirtualKeyCode").is_none());
    }

    #[test]
    fn the_navigation_and_function_keys_have_their_classic_codes() {
        for (key, code, virtual_key) in [
            ("ArrowLeft", "ArrowLeft", 37),
            ("ArrowUp", "ArrowUp", 38),
            ("ArrowRight", "ArrowRight", 39),
            ("ArrowDown", "ArrowDown", 40),
            ("Backspace", "Backspace", 8),
            ("Tab", "Tab", 9),
            ("Escape", "Escape", 27),
            ("Delete", "Delete", 46),
            ("Home", "Home", 36),
            ("End", "End", 35),
            ("PageUp", "PageUp", 33),
            ("PageDown", "PageDown", 34),
            ("F5", "F5", 116),
            ("F12", "F12", 123),
            ("Meta", "MetaLeft", 91),
            ("Control", "ControlLeft", 17),
        ] {
            let definition = key_definition(key).unwrap_or_else(|| panic!("{key} is known"));
            assert_eq!(definition.code, code, "{key}");
            assert_eq!(definition.windows_virtual_key_code, virtual_key, "{key}");
            assert!(!definition.needs_shift, "{key}");
        }
        // A named key other than Enter/Tab inserts nothing.
        assert_eq!(key_definition("ArrowLeft").expect("known").text, None);
        assert_eq!(
            key_definition("Backspace").expect("known").text,
            None,
            "backspace deletes, it does not type",
        );
    }

    #[test]
    fn every_printable_ascii_character_maps_to_something() {
        for byte in 0x20u8..0x7f {
            let character = char::from(byte);
            let key = character.to_string();
            let definition =
                key_definition(&key).unwrap_or_else(|| panic!("{character:?} has no definition"));
            assert_eq!(definition.key, key);
            assert_eq!(
                definition.text.as_deref(),
                Some(key.as_str()),
                "{character:?} must insert itself",
            );
            assert!(
                !definition.code.is_empty(),
                "{character:?} has no physical key",
            );
            assert_ne!(definition.windows_virtual_key_code, 0, "{character:?}");
        }
    }

    #[test]
    fn the_shifted_and_unshifted_halves_of_a_key_share_a_code() {
        for (lower, upper) in [
            ("1", "!"),
            ("2", "@"),
            ("9", "("),
            ("0", ")"),
            ("-", "_"),
            ("=", "+"),
            ("[", "{"),
            ("]", "}"),
            ("\\", "|"),
            (";", ":"),
            ("'", "\""),
            (",", "<"),
            (".", ">"),
            ("/", "?"),
            ("`", "~"),
        ] {
            let plain = key_definition(lower).expect("known");
            let shifted = key_definition(upper).expect("known");
            assert_eq!(plain.code, shifted.code, "{lower} vs {upper}");
            assert_eq!(
                plain.windows_virtual_key_code, shifted.windows_virtual_key_code,
                "{lower} vs {upper}",
            );
            assert!(!plain.needs_shift, "{lower}");
            assert!(shifted.needs_shift, "{upper}");
        }
    }

    #[test]
    fn space_is_a_key_and_a_multi_character_string_is_not() {
        let space = key_definition(" ").expect("known");
        assert_eq!(space.code, "Space");
        assert_eq!(space.windows_virtual_key_code, 32);
        assert_eq!(space.text.as_deref(), Some(" "));

        assert_eq!(key_definition("ab"), None, "not a key name");
        assert_eq!(key_definition(""), None);
    }

    #[test]
    fn insert_text_sends_the_whole_string_in_one_command() {
        assert_eq!(
            json(&InsertTextParams::new("hunter2".to_owned())),
            serde_json::json!({ "text": "hunter2" }),
        );
    }

    #[test]
    fn a_shortcut_key_down_sends_no_text_but_a_shifted_one_still_does() {
        // Ctrl+A is "select all", not "select all and also type an `a`".
        let select_all = KeyEvent::down("a").with_modifiers(Modifiers::CTRL);
        let params = json(&key_params(&select_all));
        assert_eq!(params["modifiers"], 2);
        assert_eq!(params["windowsVirtualKeyCode"], 65);
        assert!(params.get("text").is_none());
        assert!(params.get("unmodifiedText").is_none());

        // Shift is the exception: Shift+1 really does type `!`.
        let bang = KeyEvent::down("!");
        assert_eq!(bang.modifiers, Modifiers::SHIFT);
        assert_eq!(json(&key_params(&bang))["text"], "!");

        // …and a `char` event is always about its text, whatever is held.
        let held = KeyEvent::char("a").with_modifiers(Modifiers::META);
        assert_eq!(json(&key_params(&held))["text"], "a");
    }

    #[test]
    fn intersects_is_any_and_contains_is_all() {
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(ctrl_shift.intersects(Modifiers::CTRL | Modifiers::ALT));
        assert!(!ctrl_shift.contains(Modifiers::CTRL | Modifiers::ALT));
        assert!(!Modifiers::NONE.intersects(Modifiers::CTRL));
    }

    #[test]
    fn a_caller_can_override_what_the_table_inferred() {
        let event = KeyEvent::down("a")
            .with_code("KeyQ")
            .with_text("q")
            .with_modifiers(Modifiers::SHIFT);
        let params = json(&key_params(&event));
        assert_eq!(params["code"], "KeyQ");
        assert_eq!(params["text"], "q");
        assert_eq!(params["modifiers"], 8);
    }
}

// ---------------------------------------------------------------------------
// Live: real Chrome, a loopback page, assertions against what the page shows
// ---------------------------------------------------------------------------

#[cfg(test)]
mod live {
    use std::time::Duration;

    use super::{Button, KeyEvent, Modifiers, MouseEvent};
    use crate::config::{BrowserConfig, FetchParams};
    use crate::live_u14::require_browser;
    use crate::session::BrowserSession;

    /// A button that records that it was clicked, an input that records the
    /// keys it saw, and nothing else — every assertion below reads one of
    /// these three elements out of the live DOM.
    const PAGE: &str = "<!DOCTYPE html><html><head><title>Input</title></head>\
        <body style=\"margin:0\">\
        <button id=\"btn\" style=\"position:absolute;left:40px;top:40px;\
            width:200px;height:60px\" \
            onclick=\"document.getElementById('out').textContent='clicked at '\
                + Math.round(event.clientX) + ',' + Math.round(event.clientY)\">go</button>\
        <p id=\"out\" style=\"position:absolute;left:40px;top:120px\">idle</p>\
        <input id=\"name\" value=\"\" \
            style=\"position:absolute;left:40px;top:180px;width:300px;height:40px\" \
            onkeydown=\"document.getElementById('keys').textContent += event.key + '/'\
                + event.keyCode + ' '\">\
        <p id=\"keys\" style=\"position:absolute;left:40px;top:240px\"></p>\
        </body></html>";

    mod site {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// A running server, stopped when it is dropped.
        pub struct Site {
            port: u16,
            running: Arc<AtomicBool>,
        }

        impl Site {
            /// Bind an ephemeral loopback port and serve `body` at every path.
            pub fn start(body: &'static str) -> Self {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let port = listener.local_addr().expect("addr").port();
                let running = Arc::new(AtomicBool::new(true));

                let thread_running = Arc::clone(&running);
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        if !thread_running.load(Ordering::Acquire) {
                            break;
                        }
                        match stream {
                            Ok(stream) => {
                                std::thread::spawn(move || serve(stream, body));
                            }
                            Err(_) => break,
                        }
                    }
                });

                Self { port, running }
            }

            /// An absolute URL for `path`.
            pub fn url(&self, path: &str) -> String {
                format!("http://127.0.0.1:{}{path}", self.port)
            }
        }

        impl Drop for Site {
            fn drop(&mut self) {
                self.running.store(false, Ordering::Release);
                let _ = TcpStream::connect(("127.0.0.1", self.port));
            }
        }

        fn serve(mut stream: TcpStream, body: &'static str) {
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
                return;
            }
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {}
                    Err(_) => return,
                }
            }

            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            let mut sink = Vec::new();
            let _ = reader.read_to_end(&mut sink);
        }
    }

    /// A started session sitting on [`PAGE`], plus the tab it used.
    async fn page_session() -> (site::Site, BrowserSession, crate::cdp::CdpPage) {
        let site = site::Site::start(PAGE);
        let session = BrowserSession::new(BrowserConfig::new().headless(true).timeout_ms(20_000.0))
            .expect("a valid session config");
        session.start().await.expect("the browser starts");
        session
            .fetch(&site.url("/"), FetchParams::default())
            .await
            .expect("the page loads");
        let page = session
            .pooled_pages()
            .into_iter()
            .next()
            .expect("the tab stayed in the pool");
        (site, session, page)
    }

    /// Poll `expression` until it is `expected`, or give up.
    ///
    /// The events are asynchronous on the page's side; without a poll this
    /// would be a race dressed up as an assertion.
    async fn wait_for_text(page: &crate::cdp::CdpPage, expression: &str, expected: &str) -> String {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut last = String::new();
        while tokio::time::Instant::now() < deadline {
            last = page
                .evaluate_in_main_frame(expression)
                .await
                .expect("the expression evaluates")
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if last == expected {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        last
    }

    /// Click the centre of `selector`, the way a dashboard operator would.
    async fn click(page: &crate::cdp::CdpPage, selector: &str) -> (f64, f64) {
        let box_ = page
            .last_bounding_box(selector)
            .await
            .expect("the box is measured")
            .unwrap_or_else(|| panic!("{selector} is on the page"));
        let x = box_.x + box_.width / 2.0;
        let y = box_.y + box_.height / 2.0;
        page.dispatch_mouse(MouseEvent::moved(x, y))
            .await
            .expect("the move dispatches");
        page.dispatch_mouse(MouseEvent::pressed(x, y, Button::Left, 1))
            .await
            .expect("the press dispatches");
        page.dispatch_mouse(MouseEvent::released(x, y, Button::Left, 1))
            .await
            .expect("the release dispatches");
        (x, y)
    }

    #[tokio::test]
    async fn a_dispatched_click_really_reaches_the_element_under_it() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        assert_eq!(
            page.evaluate_in_main_frame("document.getElementById('out').textContent")
                .await
                .expect("the expression evaluates")
                .as_str(),
            Some("idle"),
            "the page starts idle, so the click below is what changes it",
        );

        let (x, y) = click(&page, "#btn").await;
        // The handler writes back the coordinates it saw, so this asserts both
        // that the click landed *and* that it landed where we aimed.
        let expected = format!("clicked at {},{}", x.round() as i64, y.round() as i64);
        let observed = wait_for_text(
            &page,
            "document.getElementById('out').textContent",
            &expected,
        )
        .await;
        session.close().await;

        assert_eq!(observed, expected);
    }

    #[tokio::test]
    async fn typed_text_lands_in_the_input_the_operator_clicked() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        // Focus the field the way a human does: by clicking it.
        click(&page, "#name").await;
        page.insert_text("hunter2")
            .await
            .expect("the text is inserted");
        // …then one more character through a `char` event, which is the part
        // that a bare `keyDown` would silently fail to do.
        page.dispatch_key(KeyEvent::char("!"))
            .await
            .expect("the char event dispatches");

        let observed =
            wait_for_text(&page, "document.getElementById('name').value", "hunter2!").await;
        session.close().await;

        assert_eq!(observed, "hunter2!", "the value the page itself reports");
    }

    #[tokio::test]
    async fn a_key_down_reaches_the_page_with_its_key_and_its_key_code() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        click(&page, "#name").await;
        for key in ["Enter", "ArrowLeft", "a"] {
            page.dispatch_key(KeyEvent::down(key))
                .await
                .expect("the keydown dispatches");
            page.dispatch_key(KeyEvent::up(key))
                .await
                .expect("the keyup dispatches");
        }

        // `event.key` and the legacy `event.keyCode`, straight off the page.
        let observed = wait_for_text(
            &page,
            "document.getElementById('keys').textContent",
            "Enter/13 ArrowLeft/37 a/65 ",
        )
        .await;
        session.close().await;

        assert_eq!(observed, "Enter/13 ArrowLeft/37 a/65 ");
    }

    /// A shortcut must not leave a character behind.
    ///
    /// Note what this does *not* claim: Chrome does not run the editing command
    /// behind Ctrl+A for a synthetic key event, so the field is not actually
    /// selected afterwards. See the module docs — that is a CDP limitation this
    /// layer deliberately does not paper over. What is asserted is the part
    /// that is ours: `keyDown` with Ctrl held sends no `text`, so pressing the
    /// shortcut does not type an `a` into the password the operator is filling.
    #[tokio::test]
    async fn a_ctrl_shortcut_does_not_type_a_character_into_the_field() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        click(&page, "#name").await;
        page.insert_text("hunter2")
            .await
            .expect("the text is inserted");
        for event in [
            KeyEvent::down("a").with_modifiers(Modifiers::CTRL),
            KeyEvent::up("a").with_modifiers(Modifiers::CTRL),
        ] {
            page.dispatch_key(event)
                .await
                .expect("the shortcut dispatches");
        }

        // The page saw the keystroke…
        let keys = wait_for_text(
            &page,
            "document.getElementById('keys').textContent",
            "a/65 ",
        )
        .await;
        // …and the field is untouched.
        let value = wait_for_text(&page, "document.getElementById('name').value", "hunter2").await;
        session.close().await;

        assert_eq!(keys, "a/65 ", "the keydown reached the page");
        assert_eq!(value, "hunter2", "Ctrl+A typed an `a` into the field");
    }

    #[tokio::test]
    async fn a_wheel_event_scrolls_the_document() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        // Make the page taller than the viewport so there is somewhere to go.
        page.evaluate_in_main_frame("document.body.style.height = '5000px'")
            .await
            .expect("the page grows");
        page.dispatch_mouse(MouseEvent::wheel(100.0, 100.0, 0.0, 400.0))
            .await
            .expect("the wheel dispatches");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut scrolled = 0.0;
        while tokio::time::Instant::now() < deadline && scrolled <= 0.0 {
            scrolled = page
                .evaluate_in_main_frame("window.scrollY")
                .await
                .expect("the expression evaluates")
                .as_f64()
                .unwrap_or_default();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        session.close().await;

        assert!(scrolled > 0.0, "window.scrollY is still {scrolled}");
    }

    /// Width and height out of a PNG's IHDR.
    fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
        if !bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            return None;
        }
        let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
        let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
        Some((width, height))
    }

    #[tokio::test]
    async fn a_screenshot_is_png_bytes_and_full_page_reaches_past_the_viewport() {
        require_browser!();
        let (_site, session, page) = page_session().await;

        // Five viewports' worth of document, so "full page" has something to
        // prove beyond what is on screen.
        page.evaluate_in_main_frame("document.body.style.height = '5000px'")
            .await
            .expect("the page grows");

        let viewport = page.screenshot(false).await.expect("the viewport capture");
        let whole = page.screenshot(true).await.expect("the full-page capture");
        session.close().await;

        let (viewport_width, viewport_height) =
            png_size(&viewport).expect("the viewport shot is a readable PNG");
        let (whole_width, whole_height) =
            png_size(&whole).expect("the full-page shot is a readable PNG");

        // Decoded bytes, not the base64 string CDP sent.
        assert_eq!(&viewport[..4], &[0x89, b'P', b'N', b'G']);
        assert!(
            viewport_height > 0 && viewport_width > 0,
            "{viewport_width}x{viewport_height}",
        );
        // Observed: 2560x1440 and 2560x10000 — the 1280x720 DIP viewport and
        // the 5000px document, each doubled by the emulated device scale
        // factor of 2. The relative assertions below hold whatever that is.
        assert_eq!(
            whole_width, viewport_width,
            "a full-page shot is taller, not wider",
        );
        assert!(
            whole_height > viewport_height * 4,
            "full_page captured {whole_height}px against a viewport's \
             {viewport_height}px — the clip did not reach past the fold",
        );
    }
}
