//! The threads' colors: a fixed palette of 30 and which one each thread gets.
//!
//! The palette ([`THREAD_PALETTE`]) is 30 xterm 256-color cube values, so a terminal with 256
//! colors draws them exactly (one with true color too). They were picked from the cube to be as
//! far apart as it allows: the closest two are 7 apart in CIEDE2000 and 0.072 in OKLab. Each
//! has a contrast of at least 3:1 against black and a dark grey (`#1e1e1e`) and at least 2:1
//! against white, and none is near a color that means something in the TUI (see
//! `twr-tui`'s `color` module). They are in the order a farthest-first walk visits them, so the
//! first threads get the most different colors.
//!
//! [`Registry`](crate::reduce::Registry) gives a thread its color ([`allocate`]) when it
//! folds its `thread.created`, from the colors of the threads alive at that point of the log,
//! and keeps it ([`ThreadView::color`](crate::reduce::ThreadView::color)): the same on every
//! client, after a restart or a reconnect, and whatever happens to other threads later. Up to
//! 30 threads alive at once never share a color; past 30 they share evenly, and since no
//! thread changes color, two that came to share one keep it until one of them goes.

/// The threads' colors, xterm 256-color indices (all in the 6×6×6 cube), in the order
/// [`allocate`] prefers them when nothing else decides.
pub const THREAD_PALETTE: [u8; 30] = [
    31, 201, 106, 211, 130, 99, 111, 28, 27, 73, 97, 169, 129, 100, 104, 95, 199, 171, 206, 134,
    34, 29, 62, 141, 204, 39, 36, 69, 146, 131,
];

/// The RGB value xterm draws cube index `n` (16 to 231) with.
pub fn cube_rgb(n: u8) -> (u8, u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let i = usize::from(n.clamp(16, 231) - 16);
    (LEVELS[i / 36], LEVELS[i / 6 % 6], LEVELS[i % 6])
}

/// `c` in OKLab (L, a, b), a space where distance follows how different colors look.
pub fn oklab(c: (u8, u8, u8)) -> (f64, f64, f64) {
    let lin = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(c.0), lin(c.1), lin(c.2));
    let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
    let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
    let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
    (
        0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s,
    )
}

/// How far apart palette colors `a` and `b` look: their OKLab distance.
pub fn distance(a: u8, b: u8) -> f64 {
    let at = |i: u8| oklab(cube_rgb(THREAD_PALETTE[usize::from(i)]));
    let ((l1, a1, b1), (l2, a2, b2)) = (at(a), at(b));
    ((l1 - l2).powi(2) + (a1 - a2).powi(2) + (b1 - b2).powi(2)).sqrt()
}

/// The color (a place in [`THREAD_PALETTE`]) for a new thread while the threads alive hold
/// `held` (one entry per thread, places out of the palette ignored): one held by the fewest
/// (none while fewer than 30 are alive), of those the one farthest from the nearest other
/// color held ([`distance`]), of those the first in the palette. More than 30 threads share
/// colors evenly, in an order that only depends on what is held.
pub fn allocate(held: impl IntoIterator<Item = u8>) -> u8 {
    let mut count = [0usize; THREAD_PALETTE.len()];
    for c in held {
        if let Some(n) = count.get_mut(usize::from(c)) {
            *n += 1;
        }
    }
    let fewest = count.iter().copied().min().unwrap_or(0);
    let mut best: Option<(u8, f64)> = None;
    for (c, n) in (0u8..).zip(count) {
        if n != fewest {
            continue;
        }
        let apart = (0u8..)
            .zip(count)
            .filter(|&(o, m)| o != c && m > 0)
            .map(|(o, _)| distance(c, o))
            .fold(f64::INFINITY, f64::min);
        if best.is_none_or(|(_, b)| apart > b) {
            best = Some((c, apart));
        }
    }
    best.map_or(0, |(c, _)| c)
}

/// Whether a thread that had color `c` keeps it while the threads alive hold `held`: no
/// color is held by fewer of them (with fewer than 30 alive: none of them holds it).
pub fn keeps(c: u8, held: impl IntoIterator<Item = u8>) -> bool {
    let mut count = [0usize; THREAD_PALETTE.len()];
    for h in held {
        if let Some(n) = count.get_mut(usize::from(h)) {
            *n += 1;
        }
    }
    count
        .get(usize::from(c))
        .is_some_and(|&n| count.iter().all(|&m| m >= n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The palette is 30 different cube colors.
    #[test]
    fn the_palette_is_thirty_different_cube_colors() {
        let set: HashSet<u8> = THREAD_PALETTE.iter().copied().collect();
        assert_eq!(set.len(), 30);
        assert!(THREAD_PALETTE.iter().all(|n| (16..=231).contains(n)));
        assert_eq!(cube_rgb(16), (0, 0, 0));
        assert_eq!(cube_rgb(31), (0, 135, 175));
        assert_eq!(cube_rgb(231), (255, 255, 255));
    }

    /// Every two colors look apart: 0.07 at least in OKLab (a just noticeable difference is
    /// about 0.02).
    #[test]
    fn every_two_colors_look_apart() {
        let n = THREAD_PALETTE.len() as u8;
        let mut nearest = f64::INFINITY;
        for a in 0..n {
            for b in a + 1..n {
                nearest = nearest.min(distance(a, b));
            }
        }
        assert!(nearest >= 0.07, "{nearest}");
        let (l, a, b) = oklab((255, 255, 255));
        assert!((l - 1.0).abs() < 1e-3 && a.abs() < 1e-3 && b.abs() < 1e-3);
    }

    /// The palette's order is a farthest-first walk: each color is the farthest from the ones
    /// before it, so allocating with nothing ever freed takes them in order.
    #[test]
    fn allocating_takes_the_palette_in_order() {
        let mut held = Vec::new();
        for want in 0..30u8 {
            let c = allocate(held.iter().copied());
            assert_eq!(c, want, "{held:?}");
            held.push(c);
        }
    }

    /// A freed color is taken again before any reuse; the next new thread takes the color
    /// farthest from those held, not the first free one.
    #[test]
    fn a_new_thread_takes_a_free_color_far_from_the_held_ones() {
        let all: Vec<u8> = (0..30).collect();
        let without = |gone: &[u8]| -> Vec<u8> {
            all.iter().copied().filter(|c| !gone.contains(c)).collect()
        };
        assert_eq!(allocate(without(&[17])), 17);
        let free = [3u8, 25];
        let c = allocate(without(&free));
        assert!(free.contains(&c));
        let held = without(&free);
        let apart = |x: u8| {
            held.iter()
                .map(|&h| distance(x, h))
                .fold(f64::INFINITY, f64::min)
        };
        let other = if c == 3 { 25 } else { 3 };
        assert!(apart(c) >= apart(other));
        // With a few held, a far one rather than the next in the palette.
        let c = allocate([0u8, 1, 2]);
        assert_eq!(c, 3);
        let c = allocate([0u8, 3]);
        assert!(c != 0 && c != 3);
        assert!(!keeps(0, [0u8, 3]) && keeps(1, [0u8, 3]));
    }

    /// Past 30 threads, colors are shared evenly: the next one takes a color held by the
    /// fewest, and the same held colors always give the same answer.
    #[test]
    fn past_thirty_threads_share_colors_evenly() {
        let mut held: Vec<u8> = Vec::new();
        for _ in 0..75 {
            let c = allocate(held.iter().copied());
            assert_eq!(c, allocate(held.iter().copied()));
            held.push(c);
            let mut count = [0usize; 30];
            for h in &held {
                count[usize::from(*h)] += 1;
            }
            let (lo, hi) = (count.iter().min().unwrap(), count.iter().max().unwrap());
            assert!(hi - lo <= 1, "{count:?}");
        }
        let held: Vec<u8> = (0..30).chain([5]).collect();
        assert!(keeps(6, held.iter().copied()));
        assert!(!keeps(5, held.iter().copied()));
        // Out of the palette: ignored, never kept.
        assert_eq!(allocate([200u8]), 0);
        assert!(!keeps(200, []));
    }
}
