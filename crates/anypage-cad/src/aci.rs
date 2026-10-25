//! AutoCAD Color Index (ACI) to RGB, the standard 256-entry palette.
//!
//! 1–9 are fixed, 10–249 are 24 hues × 5 brightness levels × 2 saturations,
//! 250–255 are greys. Index 7 is "white or black": it is drawn in the
//! foreground colour that contrasts with the background.

const BASIC: [[u8; 3]; 10] = [
    [0, 0, 0],       // 0 = BYBLOCK, never drawn as such
    [255, 0, 0],     // 1 red
    [255, 255, 0],   // 2 yellow
    [0, 255, 0],     // 3 green
    [0, 255, 255],   // 4 cyan
    [0, 0, 255],     // 5 blue
    [255, 0, 255],   // 6 magenta
    [255, 255, 255], // 7 white / black
    [128, 128, 128], // 8
    [192, 192, 192], // 9
];

const GREYS: [u8; 6] = [51, 80, 105, 130, 190, 255];
const LEVELS: [f64; 5] = [255.0, 165.0, 127.0, 76.0, 38.0];

fn hue_rgb(hue_deg: f64) -> [f64; 3] {
    // HSV with S = V = 1, channel values floored like the AutoCAD table
    let h = hue_deg.rem_euclid(360.0) / 60.0;
    let f = |x: f64| (x * 255.0).floor();
    let frac = h - h.floor();
    match h.floor() as u32 {
        0 => [255.0, f(frac), 0.0],
        1 => [f(1.0 - frac), 255.0, 0.0],
        2 => [0.0, 255.0, f(frac)],
        3 => [0.0, f(1.0 - frac), 255.0],
        4 => [f(frac), 0.0, 255.0],
        _ => [255.0, 0.0, f(1.0 - frac)],
    }
}

/// RGB of an ACI index (1–255). 0 and 256+ are not colours (BYBLOCK,
/// BYLAYER) and map to white.
pub fn rgb(index: u8) -> [u8; 3] {
    match index {
        0..=9 => BASIC[index as usize],
        250..=255 => {
            let g = GREYS[(index - 250) as usize];
            [g, g, g]
        }
        _ => {
            let i = index as u32;
            let hue = ((i / 10) - 1) as f64 * 15.0;
            let variant = i % 10;
            let mut c = hue_rgb(hue);
            if variant % 2 == 1 {
                for x in c.iter_mut() {
                    *x += ((255.0 - *x) / 2.0).floor();
                }
            }
            let level = LEVELS[(variant / 2) as usize];
            [
                (c[0] * level / 255.0).round() as u8,
                (c[1] * level / 255.0).round() as u8,
                (c[2] * level / 255.0).round() as u8,
            ]
        }
    }
}

/// The nearest ACI index (1–255, never 7) to an RGB colour, for writing a
/// fallback index next to a true colour.
pub fn nearest(c: [u8; 3]) -> u8 {
    let mut best = (u32::MAX, 1u8);
    for i in 1..=255u8 {
        if i == 7 {
            continue;
        }
        let q = rgb(i);
        let d = (0..3).map(|k| (q[k] as i32 - c[k] as i32).pow(2) as u32).sum::<u32>();
        if d < best.0 {
            best = (d, i);
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_entries() {
        assert_eq!(rgb(1), [255, 0, 0]);
        assert_eq!(rgb(5), [0, 0, 255]);
        assert_eq!(rgb(10), [255, 0, 0]);
        assert_eq!(rgb(11), [255, 127, 127]);
        assert_eq!(rgb(12), [165, 0, 0]);
        assert_eq!(rgb(13), [165, 82, 82]);
        assert_eq!(rgb(17), [76, 38, 38]);
        assert_eq!(rgb(19), [38, 19, 19]);
        assert_eq!(rgb(20), [255, 63, 0]);
        assert_eq!(rgb(21), [255, 159, 127]);
        assert_eq!(rgb(22), [165, 41, 0]);
        assert_eq!(rgb(30), [255, 127, 0]);
        assert_eq!(rgb(40), [255, 191, 0]);
        assert_eq!(rgb(50), [255, 255, 0]);
        assert_eq!(rgb(140), [0, 191, 255]);
        assert_eq!(rgb(250), [51, 51, 51]);
        assert_eq!(rgb(255), [255, 255, 255]);
    }

    #[test]
    fn nearest_round_trips() {
        for i in [1u8, 3, 5, 30, 140, 250] {
            assert_eq!(rgb(nearest(rgb(i))), rgb(i));
        }
    }
}
