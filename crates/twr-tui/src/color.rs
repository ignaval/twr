//! The colors of threads and how deep the TUI's colors go.
//!
//! Every thread has a color of its own ([`thread_color`]): its card's border and name, its
//! boxes and name in the orchestrator's conversation, its requests' boxes, and the header band
//! and input box while it is dived into. The palette ([`THREAD_COLORS`]) is the 30 fixed
//! 256-color values of [`twr_core::palette`], and the hub's log decides which one a thread has
//! ([`twr_core::reduce::ThreadView::color`]): up to 30 threads alive never share one (unless
//! more than 30 were alive at once since), and a thread keeps its own through renames,
//! restarts and other threads coming and going. Each
//! reads on dark and light terminals (3:1 at least against black and a dark grey, 2:1 against
//! white), and none is near a color that means something (the orchestrator's cyan, a dive's
//! magenta, the working icon's orange, the selection's yellow, red errors, green running, blue
//! done, the muted and dark greys, code's light blue), the alarming ones (red, orange, yellow)
//! least of all: its warm colors are pinks, a brown and muted roses.
//!
//! `[tui] colors` ([`ColorMode`]) says how deep the colors go, resolved once at start
//! ([`resolve`]: `auto` from `NO_COLOR`, `COLORTERM` and `TERM`) into a [`Depth`]. The view is
//! drawn in full color and then brought down to that depth in one pass over the frame
//! ([`degrade`]): with 16 colors, a thread's color is the old dark grey (a fill the old
//! magenta) and other colors the nearest named one of their kind ([`basic`]), a text that
//! would come down to its fill's color taking one that reads on it ([`apart`]); with none,
//! every cell is in the terminal's own colors, a filled cell drawn in reverse video.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use twr_core::config::ColorMode;
use twr_core::palette::THREAD_PALETTE;
use twr_core::reduce::ThreadView;

/// The threads' colors, [`twr_core::palette::THREAD_PALETTE`] as 256-color values. Their
/// numbers are used for nothing else, so a 16-color terminal can tell a thread's color from any
/// other ([`degrade`]).
pub const THREAD_COLORS: [Color; THREAD_PALETTE.len()] = {
    let mut colors = [Color::Reset; THREAD_PALETTE.len()];
    let mut i = 0;
    while i < colors.len() {
        colors[i] = Color::Indexed(THREAD_PALETTE[i]);
        i += 1;
    }
    colors
};

/// Thread `t`'s color: the one of [`THREAD_COLORS`] its view holds
/// ([`ThreadView::color`]); for a view without one (an orchestrator's, or one no registry
/// folded), one picked from its id (FNV-1a of its bytes), the same on every frame and run.
pub fn thread_color(t: &ThreadView) -> Color {
    let at = t.color.map_or_else(
        || {
            let mut h: u32 = 0x811c_9dc5;
            for b in t.id.bytes() {
                h ^= u32::from(b);
                h = h.wrapping_mul(0x0100_0193);
            }
            h as usize
        },
        usize::from,
    );
    THREAD_COLORS[at % THREAD_COLORS.len()]
}

/// Black or white, whichever reads better on `bg` (the higher contrast): the text of a band
/// filled with a thread's color. Black for a color it cannot measure.
pub fn text_on(bg: Color) -> Color {
    let (black, white) = (Color::Indexed(16), Color::Indexed(231));
    match rgb(bg) {
        Some(c) if contrast(c, (255, 255, 255)) > contrast(c, (0, 0, 0)) => white,
        _ => black,
    }
}

/// How deep the TUI's colors go, `[tui] colors` resolved ([`resolve`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    /// 256 colors (and true color where the view asks for it): drawn as it is.
    #[default]
    Full,
    /// The 16 named colors.
    Basic,
    /// None: the terminal's own colors, with bold, dim, reverse video and border shapes.
    None,
}

/// `mode` resolved against the environment's `NO_COLOR`, `COLORTERM` and `TERM` (`None` when
/// unset): an explicit mode as it says; `auto` none when `NO_COLOR` is set and not empty,
/// else 256 colors when `COLORTERM` is set and not empty or `TERM` says so (`256color`,
/// `direct`, `truecolor` in it), else 16 for a `TERM` known to have only those (`linux`,
/// `vt100`, `xterm`, `screen`, `dumb`, ...), else 256.
pub fn resolve(
    mode: ColorMode,
    no_color: Option<&str>,
    colorterm: Option<&str>,
    term: Option<&str>,
) -> Depth {
    const BASIC: [&str; 11] = [
        "linux", "vt100", "vt102", "vt220", "ansi", "xterm", "screen", "tmux", "rxvt", "cons25",
        "dumb",
    ];
    match mode {
        ColorMode::Full => return Depth::Full,
        ColorMode::Basic => return Depth::Basic,
        ColorMode::None => return Depth::None,
        ColorMode::Auto => {}
    }
    let set = |v: Option<&str>| v.is_some_and(|v| !v.is_empty());
    if set(no_color) {
        return Depth::None;
    }
    if set(colorterm) {
        return Depth::Full;
    }
    let term = term.unwrap_or("");
    if ["256color", "direct", "truecolor"]
        .iter()
        .any(|w| term.contains(w))
    {
        Depth::Full
    } else if BASIC.contains(&term) {
        Depth::Basic
    } else {
        Depth::Full
    }
}

/// [`resolve`] with this process's environment.
pub fn resolve_env(mode: ColorMode) -> Depth {
    let var = |k: &str| std::env::var(k).ok();
    resolve(
        mode,
        var("NO_COLOR").as_deref(),
        var("COLORTERM").as_deref(),
        var("TERM").as_deref(),
    )
}

/// Brings the frame in `buf` down to `depth`. [`Depth::Full`] leaves it. [`Depth::Basic`]
/// draws a thread's color as dark grey (a cell filled with one as magenta: a dive's band, as
/// it was before threads had colors) and every other color as one of the 16 named ones
/// ([`basic`]); a cell whose text and fill differ but come down to the same named color (a
/// dimmed grey on a grey band: both dark grey) has its text in one that reads on the fill
/// ([`apart`]).
/// [`Depth::None`] puts every cell in the terminal's own colors and reverses the video of
/// each cell that had a fill (a band, a user message, a request's badge), so fills stay
/// seen; a cell already reversed on a fill (the mouse selection over a band) is drawn plain.
/// Modifiers stay, and so do the border shapes (a selected card's thick border, a dived
/// thread's double one).
pub fn degrade(buf: &mut Buffer, depth: Depth) {
    match depth {
        Depth::Full => {}
        Depth::Basic => {
            for cell in &mut buf.content {
                let (fg, bg) = (basic(cell.fg, false), basic(cell.bg, true));
                cell.fg = if fg == bg && cell.fg != cell.bg {
                    apart(bg)
                } else {
                    fg
                };
                cell.bg = bg;
            }
        }
        Depth::None => {
            for cell in &mut buf.content {
                if cell.bg != Color::Reset {
                    cell.modifier.toggle(Modifier::REVERSED);
                }
                cell.fg = Color::Reset;
                cell.bg = Color::Reset;
            }
        }
    }
}

/// `c`, a foreground or (`bg`) a background, in 16 colors. One of [`THREAD_COLORS`] is dark
/// grey, or magenta as a background. Any other 256-color or RGB value is the nearest named
/// color ([`NAMED`]) of its kind: a color with a hue the nearest of the twelve with one (an
/// orange stays warm, a light magenta a magenta, never a grey), a grey the nearest grey, black
/// or white, except that a background grey other than black is never black (a band stays seen
/// on a dark terminal). The 16 first indices are their named colors; named colors and `Reset`
/// stay as they are.
fn basic(c: Color, bg: bool) -> Color {
    if THREAD_COLORS.contains(&c) {
        return if bg { Color::Magenta } else { Color::DarkGray };
    }
    let v = match c {
        Color::Indexed(n) if n < 16 => return NAMED[usize::from(n)].0,
        Color::Indexed(_) | Color::Rgb(..) => match rgb(c) {
            Some(v) => v,
            None => return c,
        },
        other => return other,
    };
    let neutral = |n: (u8, u8, u8)| {
        let (hi, lo) = (n.0.max(n.1).max(n.2), n.0.min(n.1).min(n.2));
        hi - lo < 16
    };
    let d = |n: (u8, u8, u8)| {
        let sq = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        sq(v.0, n.0) + sq(v.1, n.1) + sq(v.2, n.2)
    };
    let grey = neutral(v);
    NAMED
        .iter()
        .filter(|(_, n)| neutral(*n) == grey)
        .filter(|(named, _)| !(grey && bg && v != (0, 0, 0) && *named == Color::Black))
        .min_by_key(|(_, n)| d(*n))
        .map_or(Color::Reset, |(named, _)| *named)
}

/// A named text color that reads on named fill `bg` and is not it: light grey on a dark fill
/// (dark grey, black, blue, red: not white, so a dimmed text stays dimmer than a white one),
/// black on a light one.
fn apart(bg: Color) -> Color {
    match rgb(bg) {
        Some(v) if luminance(v) < 0.25 => Color::Gray,
        _ => Color::Black,
    }
}

/// The 16 named colors with xterm's default RGB values, in ANSI order (index 0 to 15).
const NAMED: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (205, 0, 0)),
    (Color::Green, (0, 205, 0)),
    (Color::Yellow, (205, 205, 0)),
    (Color::Blue, (0, 0, 238)),
    (Color::Magenta, (205, 0, 205)),
    (Color::Cyan, (0, 205, 205)),
    (Color::Gray, (229, 229, 229)),
    (Color::DarkGray, (127, 127, 127)),
    (Color::LightRed, (255, 0, 0)),
    (Color::LightGreen, (0, 255, 0)),
    (Color::LightYellow, (255, 255, 0)),
    (Color::LightBlue, (92, 92, 255)),
    (Color::LightMagenta, (255, 0, 255)),
    (Color::LightCyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

/// The RGB value xterm draws 256-color index `n` with: the 16 named colors ([`NAMED`]), the
/// 6×6×6 cube (16 to 231) and the 24 greys (232 to 255).
pub fn xterm_rgb(n: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match n {
        0..=15 => NAMED[usize::from(n)].1,
        16..=231 => {
            let i = usize::from(n - 16);
            (LEVELS[i / 36], LEVELS[i / 6 % 6], LEVELS[i % 6])
        }
        _ => {
            let v = 8 + 10 * (n - 232);
            (v, v, v)
        }
    }
}

/// The RGB value of `c` as xterm draws it, `None` for `Reset`.
pub fn rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Reset => None,
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(n) => Some(xterm_rgb(n)),
        named => NAMED.iter().find(|(n, _)| *n == named).map(|(_, v)| *v),
    }
}

/// The WCAG relative luminance of `c` (0 for black, 1 for white).
pub fn luminance(c: (u8, u8, u8)) -> f64 {
    let lin = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
}

/// The WCAG contrast ratio of `a` and `b` (1 to 21).
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use std::collections::HashSet;

    const BLACK: (u8, u8, u8) = (0, 0, 0);
    const WHITE: (u8, u8, u8) = (255, 255, 255);

    /// How far apart two colors look: their OKLab distance ([`twr_core::palette::oklab`]).
    fn distance(a: Color, b: Color) -> f64 {
        let (a, b) = (
            twr_core::palette::oklab(rgb(a).unwrap()),
            twr_core::palette::oklab(rgb(b).unwrap()),
        );
        ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt()
    }

    /// The table is xterm's: a few known values from each part.
    #[test]
    fn the_xterm_table_has_the_cube_and_the_greys() {
        assert_eq!(xterm_rgb(16), (0, 0, 0));
        assert_eq!(xterm_rgb(80), (95, 215, 215));
        assert_eq!(xterm_rgb(176), (215, 135, 215));
        assert_eq!(xterm_rgb(231), (255, 255, 255));
        assert_eq!(xterm_rgb(232), (8, 8, 8));
        assert_eq!(xterm_rgb(246), (148, 148, 148));
        assert_eq!(xterm_rgb(255), (238, 238, 238));
        assert_eq!(xterm_rgb(5), (205, 0, 205));
        assert!((contrast(BLACK, WHITE) - 21.0).abs() < 1e-9);
    }

    /// The palette is the hub's, cube color for cube color.
    #[test]
    fn the_palette_is_the_cores() {
        for (c, n) in THREAD_COLORS.iter().zip(THREAD_PALETTE) {
            assert_eq!(*c, Color::Indexed(n));
            assert_eq!(xterm_rgb(n), twr_core::palette::cube_rgb(n));
        }
    }

    /// A thread's color is the one its view holds; a view without one gets one from its id
    /// (a fixed hash, not the process's random one), and ids spread over the whole palette.
    #[test]
    fn a_threads_color_is_the_one_it_holds_or_one_from_its_id() {
        let view = |id: &str, color: Option<u8>| ThreadView {
            color,
            ..ThreadView::new(id)
        };
        for at in 0..THREAD_COLORS.len() as u8 {
            assert_eq!(
                thread_color(&view("t-alpha", Some(at))),
                THREAD_COLORS[usize::from(at)]
            );
        }
        assert_eq!(
            thread_color(&view("t-alpha", Some(200))),
            THREAD_COLORS[200 % 30]
        );
        let alpha = view("t-alpha", None);
        assert_eq!(thread_color(&alpha), thread_color(&alpha.clone()));
        // FNV-1a pinned: these never change from one run or build to the next.
        assert_eq!(thread_color(&alpha), Color::Indexed(104));
        assert_eq!(thread_color(&view("t-beta", None)), Color::Indexed(62));
        let used: HashSet<Color> = (0..400)
            .map(|i| thread_color(&view(&format!("t-{i}"), None)))
            .collect();
        assert_eq!(used.len(), THREAD_COLORS.len(), "{used:?}");
    }

    /// Every thread color reads on a dark terminal (3:1 at least against black and against a
    /// dark grey, `#1e1e1e`) and shows on a light one (2:1 against white): a border and a bold
    /// name. 3:1 against white too would leave fewer than 30 colors in the cube that are far
    /// enough apart.
    #[test]
    fn every_thread_color_reads_on_dark_and_shows_on_light() {
        for c in THREAD_COLORS {
            let v = rgb(c).unwrap();
            assert!(contrast(v, BLACK) >= 3.0, "{c:?} on black");
            assert!(contrast(v, (30, 30, 30)) >= 3.0, "{c:?} on dark grey");
            assert!(contrast(v, WHITE) >= 2.0, "{c:?} on white");
        }
    }

    /// No thread's color is near a color that alarms: red (an error, a permission), the
    /// working icon's orange, yellow (a question, a selection): 0.12 at least in OKLab. Nor is
    /// any a grey: every one has a hue.
    #[test]
    fn no_thread_color_is_near_an_alarm_or_a_grey() {
        let alarm = [
            Color::Red,
            Color::LightRed,
            Color::Indexed(161),
            crate::icon::ACCENT,
            Color::Yellow,
            Color::LightYellow,
            Color::Indexed(220),
        ];
        for c in THREAD_COLORS {
            for m in alarm {
                assert!(distance(c, m) >= 0.12, "{c:?} near {m:?}");
            }
            let (r, g, b) = rgb(c).unwrap();
            assert!(r.max(g).max(b) - r.min(g).min(b) >= 40, "{c:?} is a grey");
        }
    }

    /// A thread's color is far from every color that means something (0.08 in OKLab), and
    /// from the other threads' colors (0.07: a just noticeable difference is about 0.02).
    #[test]
    fn thread_colors_are_apart_from_the_meaning_colors_and_each_other() {
        let meaning = [
            Color::Indexed(80),
            Color::Cyan,
            Color::Magenta,
            Color::Indexed(176),
            crate::icon::ACCENT,
            Color::Yellow,
            Color::Indexed(220),
            Color::Red,
            Color::LightRed,
            Color::Indexed(161),
            Color::Green,
            Color::Blue,
            Color::Gray,
            Color::Indexed(246),
            Color::Indexed(153),
            Color::DarkGray,
        ];
        for c in THREAD_COLORS {
            for m in meaning {
                assert!(distance(c, m) >= 0.08, "{c:?} near {m:?}");
            }
        }
        for (i, a) in THREAD_COLORS.iter().enumerate() {
            for b in &THREAD_COLORS[i + 1..] {
                assert!(distance(*a, *b) >= 0.07, "{a:?} near {b:?}");
            }
        }
    }

    /// A band filled with a thread's color has its text in black or white, whichever reads
    /// better, at 4.5:1 at least.
    #[test]
    fn a_band_in_a_threads_color_has_readable_text() {
        for c in THREAD_COLORS {
            let fg = text_on(c);
            assert!(fg == Color::Indexed(16) || fg == Color::Indexed(231));
            let ratio = contrast(rgb(fg).unwrap(), rgb(c).unwrap());
            assert!(ratio >= 4.5, "{c:?}: {ratio}");
            let other = if fg == Color::Indexed(16) {
                WHITE
            } else {
                BLACK
            };
            assert!(ratio >= contrast(other, rgb(c).unwrap()), "{c:?}");
        }
    }

    /// The palette's numbers are used nowhere else in what the TUI draws: a cell in one of
    /// them is a thread's color, which 16 colors draw as dark grey.
    #[test]
    fn the_palettes_numbers_are_used_for_nothing_else() {
        let sources = [
            include_str!("ui.rs"),
            include_str!("md.rs"),
            include_str!("icon.rs"),
        ];
        for c in THREAD_COLORS {
            let Color::Indexed(n) = c else {
                panic!("{c:?}");
            };
            let text = format!("Indexed({n})");
            for src in sources {
                assert!(!src.contains(&text), "{text} is used elsewhere");
            }
        }
    }

    /// `auto` reads the environment: `NO_COLOR` (not empty) wins, then `COLORTERM`, then
    /// what `TERM` says; an explicit mode is what it says whatever the environment.
    #[test]
    fn the_color_depth_comes_from_the_config_or_the_environment() {
        let auto = |no: Option<&str>, ct: Option<&str>, term: Option<&str>| {
            resolve(ColorMode::Auto, no, ct, term)
        };
        assert_eq!(
            auto(Some("1"), Some("truecolor"), Some("xterm-256color")),
            Depth::None
        );
        assert_eq!(auto(Some(""), None, Some("xterm")), Depth::Basic);
        assert_eq!(auto(None, Some("truecolor"), Some("xterm")), Depth::Full);
        assert_eq!(auto(None, Some(""), Some("xterm")), Depth::Basic);
        for term in [
            "xterm-256color",
            "tmux-256color",
            "xterm-direct",
            "foo-truecolor",
        ] {
            assert_eq!(auto(None, None, Some(term)), Depth::Full, "{term}");
        }
        for term in [
            "linux", "vt100", "vt102", "vt220", "ansi", "xterm", "screen", "tmux", "rxvt",
            "cons25", "dumb",
        ] {
            assert_eq!(auto(None, None, Some(term)), Depth::Basic, "{term}");
        }
        for term in ["xterm-kitty", "alacritty", "screen.xterm-new"] {
            assert_eq!(auto(None, None, Some(term)), Depth::Full, "{term}");
        }
        assert_eq!(auto(None, None, None), Depth::Full);
        let env = (Some("1"), Some("truecolor"), Some("linux"));
        for (mode, depth) in [
            (ColorMode::Full, Depth::Full),
            (ColorMode::Basic, Depth::Basic),
            (ColorMode::None, Depth::None),
        ] {
            assert_eq!(resolve(mode, env.0, env.1, env.2), depth);
        }
    }

    /// With 16 colors, a thread's color is dark grey (a fill of it magenta), any other
    /// 256-color or RGB value the nearest named color of its kind (a color with a hue one with
    /// a hue, a grey a grey), a grey fill never black, and named colors stay; a text that
    /// differs from its fill never comes down to the fill's color; full color leaves the frame
    /// alone.
    #[test]
    fn sixteen_colors_draw_a_threads_color_as_grey_and_the_rest_named() {
        let cells = [
            (THREAD_COLORS[0], THREAD_COLORS[3]),
            (Color::Indexed(16), Color::Indexed(80)),
            (Color::Indexed(231), Color::Indexed(220)),
            (Color::Rgb(250, 10, 10), Color::Reset),
            (Color::Yellow, Color::Blue),
            (Color::Indexed(4), Color::Indexed(246)),
            // The working icon's orange, the fallback dive fill, code's light blue.
            (crate::icon::ACCENT, Color::Indexed(176)),
            (Color::Indexed(153), Color::Indexed(16)),
            // The user message's band, the compact tabs' fill, a near-black fill.
            (Color::Indexed(237), Color::Indexed(237)),
            (Color::Indexed(252), Color::Indexed(238)),
            (Color::Indexed(245), Color::Indexed(232)),
            (Color::Indexed(66), Color::Indexed(254)),
            // Text and fill that come down to one color: a taken-back message's dimmed text
            // and its marker on the band, a light grey on a lighter one; the same color
            // both ways (drawn so on purpose) stays so.
            (Color::Indexed(245), Color::Indexed(237)),
            (Color::Indexed(246), Color::Indexed(237)),
            (Color::Indexed(253), Color::Indexed(250)),
            (Color::Indexed(238), Color::Indexed(238)),
        ];
        let mut buf = Buffer::empty(Rect::new(0, 0, cells.len() as u16, 1));
        for (x, (fg, bg)) in cells.iter().enumerate() {
            let cell = &mut buf[(x as u16, 0)];
            cell.fg = *fg;
            cell.bg = *bg;
        }
        let full = buf.clone();
        degrade(&mut buf, Depth::Full);
        assert_eq!(buf, full);
        degrade(&mut buf, Depth::Basic);
        let got: Vec<(Color, Color)> = (0..cells.len() as u16)
            .map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg))
            .collect();
        assert_eq!(
            got,
            [
                (Color::DarkGray, Color::Magenta),
                (Color::Black, Color::Cyan),
                (Color::White, Color::LightYellow),
                (Color::LightRed, Color::Reset),
                (Color::Yellow, Color::Blue),
                (Color::Blue, Color::DarkGray),
                (Color::Yellow, Color::Magenta),
                (Color::LightBlue, Color::Black),
                (Color::Black, Color::DarkGray),
                (Color::Gray, Color::DarkGray),
                (Color::Gray, Color::DarkGray),
                (Color::LightBlue, Color::Gray),
                (Color::Gray, Color::DarkGray),
                (Color::Gray, Color::DarkGray),
                (Color::Black, Color::Gray),
                (Color::DarkGray, Color::DarkGray),
            ]
        );
        let grey = [Color::Black, Color::White, Color::Gray, Color::DarkGray];
        for n in 16..=255u8 {
            let c = Color::Indexed(n);
            if THREAD_COLORS.contains(&c) {
                continue;
            }
            let (r, g, b) = xterm_rgb(n);
            let hued = r.max(g).max(b) - r.min(g).min(b) >= 16;
            for bg in [false, true] {
                let named = basic(c, bg);
                assert_eq!(!grey.contains(&named), hued, "{n} {bg}: {named:?}");
                if bg && n != 16 {
                    assert_ne!(named, Color::Black, "{n}");
                }
            }
        }
    }

    /// With 16 colors no text comes down to its fill's color unless it had it: every pair of
    /// 256-color values (and the terminal's own), each cell its own pair.
    #[test]
    fn sixteen_colors_never_draw_a_text_in_its_fills_color() {
        let colors: Vec<Color> = std::iter::once(Color::Reset)
            .chain((0..=255u8).map(Color::Indexed))
            .collect();
        let n = colors.len() as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, n, n));
        for (y, bg) in colors.iter().enumerate() {
            for (x, fg) in colors.iter().enumerate() {
                let cell = &mut buf[(x as u16, y as u16)];
                (cell.fg, cell.bg) = (*fg, *bg);
            }
        }
        let full = buf.clone();
        degrade(&mut buf, Depth::Basic);
        for (was, cell) in full.content.iter().zip(&buf.content) {
            if was.fg != was.bg {
                assert_ne!(cell.fg, cell.bg, "{:?} on {:?}", was.fg, was.bg);
            }
        }
    }

    /// With no color, every cell is in the terminal's own colors; a filled one is reversed (a
    /// reversed one on a fill, plain), and the other modifiers stay.
    #[test]
    fn no_color_reverses_fills_and_keeps_the_modifiers() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        let set = |buf: &mut Buffer, x: u16, fg: Color, bg: Color, m: Modifier| {
            let cell = &mut buf[(x, 0)];
            cell.fg = fg;
            cell.bg = bg;
            cell.modifier = m;
        };
        set(&mut buf, 0, THREAD_COLORS[1], Color::Reset, Modifier::BOLD);
        set(
            &mut buf,
            1,
            Color::Indexed(16),
            THREAD_COLORS[1],
            Modifier::BOLD,
        );
        set(
            &mut buf,
            2,
            Color::Red,
            Color::Indexed(220),
            Modifier::REVERSED,
        );
        set(&mut buf, 3, Color::Reset, Color::Reset, Modifier::DIM);
        degrade(&mut buf, Depth::None);
        let got: Vec<(Color, Color, Modifier)> = (0..4)
            .map(|x| (buf[(x, 0)].fg, buf[(x, 0)].bg, buf[(x, 0)].modifier))
            .collect();
        let r = Color::Reset;
        assert_eq!(
            got,
            [
                (r, r, Modifier::BOLD),
                (r, r, Modifier::BOLD | Modifier::REVERSED),
                (r, r, Modifier::empty()),
                (r, r, Modifier::DIM),
            ]
        );
    }
}
