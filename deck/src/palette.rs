//! Two colours from a cover, the TV app's ArtColors: an accent bright enough to
//! fill a focused button on a dark screen, and a shade dark enough to sit
//! behind white text. The TV app reads them with AndroidX Palette; this is the
//! same method in small: a median-cut quantiser over a 5-bit histogram, the
//! same six targets and the same scoring.

/// An sRGB colour, 0-255 per channel.
pub type Rgb = [u8; 3];

#[derive(Clone, Copy, Debug)]
struct Swatch {
    rgb: Rgb,
    population: u32,
    hsl: [f32; 3],
}

/// Accent and shade, or nothing for an image with no usable colour.
pub fn art_colors(pixels: &[[u8; 4]]) -> Option<(Rgb, Rgb)> {
    let swatches = quantize(pixels, 16);
    if swatches.is_empty() {
        return None;
    }
    let palette = Palette::from(&swatches);
    let vivid = palette.vibrant.or(palette.light_vibrant).or(palette.dominant)?;
    let dark = palette.dark_muted.or(palette.dark_vibrant).or(palette.dominant).unwrap_or(vivid);
    Some((readable_accent(vivid), deep_shade(dark)))
}

/// Lifted toward white until it reads as a fill on a dark screen.
pub fn readable_accent(color: Rgb) -> Rgb {
    let mut c = color;
    for _ in 0..6 {
        if luminance(c) < 0.3 {
            c = mix(c, [255, 255, 255], 0.25);
        }
    }
    c
}

/// Pressed toward black, so white text over it keeps its contrast.
pub fn deep_shade(color: Rgb) -> Rgb {
    mix(color, [0, 0, 0], 0.6)
}

/// Text on an accent fill: dark on a light accent, light on a dark one.
pub fn on_accent(accent: Rgb) -> Rgb {
    if luminance(accent) > 0.4 { [0x11, 0x11, 0x11] } else { [255, 255, 255] }
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [f(a[0], b[0]), f(a[1], b[1]), f(a[2], b[2])]
}

/// Relative luminance, as Compose's `Color.luminance()`.
pub fn luminance(c: Rgb) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

fn hsl(c: Rgb) -> [f32; 3] {
    let (r, g, b) = (c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if max == min {
        return [0.0, 0.0, l];
    }
    let d = max - min;
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    [h, s, l]
}

/// Palette's default filter: not near black, not near white, and not the
/// "red-I line" Palette leaves out as skin tone.
fn allowed(hsl: [f32; 3]) -> bool {
    let [h, s, l] = hsl;
    l > 0.05 && l < 0.95 && !((10.0..=37.0).contains(&h) && s <= 0.82)
}

/// Median cut over a 5-bit-per-channel histogram, into at most `max` swatches.
fn quantize(pixels: &[[u8; 4]], max: usize) -> Vec<Swatch> {
    let mut histogram = std::collections::HashMap::<u16, u32>::new();
    for p in pixels {
        if p[3] < 128 {
            continue;
        }
        let key = ((p[0] as u16 >> 3) << 10) | ((p[1] as u16 >> 3) << 5) | (p[2] as u16 >> 3);
        *histogram.entry(key).or_default() += 1;
    }
    let mut colors: Vec<(Rgb, u32)> = histogram
        .into_iter()
        .map(|(k, n)| {
            let up = |v: u16| ((v << 3) | (v >> 2)) as u8;
            ([up((k >> 10) & 31), up((k >> 5) & 31), up(k & 31)], n)
        })
        .filter(|(rgb, _)| allowed(hsl(*rgb)))
        .collect();
    if colors.is_empty() {
        return Vec::new();
    }
    if colors.len() <= max {
        return colors.into_iter().map(|(rgb, population)| Swatch { rgb, population, hsl: hsl(rgb) }).collect();
    }
    // Boxes are index ranges into `colors`; split the largest by volume first.
    let mut boxes: Vec<(usize, usize)> = vec![(0, colors.len())];
    while boxes.len() < max {
        let Some((at, _)) = boxes
            .iter()
            .enumerate()
            .filter(|(_, (a, b))| b - a > 1)
            .max_by_key(|(_, (a, b))| volume(&colors[*a..*b]))
        else {
            break;
        };
        let (a, b) = boxes.swap_remove(at);
        let slice = &mut colors[a..b];
        let channel = longest_channel(slice);
        slice.sort_by_key(|(rgb, _)| rgb[channel]);
        let total: u32 = slice.iter().map(|(_, n)| n).sum();
        let mut running = 0;
        let mut split = slice.len() / 2;
        for (i, (_, n)) in slice.iter().enumerate() {
            running += n;
            if running >= total / 2 {
                split = (i + 1).clamp(1, slice.len() - 1);
                break;
            }
        }
        boxes.push((a, a + split));
        boxes.push((a + split, b));
    }
    boxes
        .into_iter()
        .map(|(a, b)| {
            let slice = &colors[a..b];
            let population: u32 = slice.iter().map(|(_, n)| n).sum();
            let avg = |i: usize| {
                (slice.iter().map(|(c, n)| c[i] as u64 * *n as u64).sum::<u64>() / population.max(1) as u64) as u8
            };
            let rgb = [avg(0), avg(1), avg(2)];
            Swatch { rgb, population, hsl: hsl(rgb) }
        })
        .collect()
}

fn ranges(colors: &[(Rgb, u32)]) -> [u8; 3] {
    let mut lo = [255u8; 3];
    let mut hi = [0u8; 3];
    for (c, _) in colors {
        for i in 0..3 {
            lo[i] = lo[i].min(c[i]);
            hi[i] = hi[i].max(c[i]);
        }
    }
    [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]]
}

fn volume(colors: &[(Rgb, u32)]) -> u32 {
    let r = ranges(colors);
    (r[0] as u32 + 1) * (r[1] as u32 + 1) * (r[2] as u32 + 1)
}

fn longest_channel(colors: &[(Rgb, u32)]) -> usize {
    let r = ranges(colors);
    if r[0] >= r[1] && r[0] >= r[2] {
        0
    } else if r[1] >= r[2] {
        1
    } else {
        2
    }
}

struct Target {
    lightness: (f32, f32, f32),
    saturation: (f32, f32, f32),
}

const LIGHT_VIBRANT: Target = Target { lightness: (0.55, 0.74, 1.0), saturation: (0.35, 1.0, 1.0) };
const VIBRANT: Target = Target { lightness: (0.3, 0.5, 0.7), saturation: (0.35, 1.0, 1.0) };
const DARK_VIBRANT: Target = Target { lightness: (0.0, 0.26, 0.45), saturation: (0.35, 1.0, 1.0) };
const LIGHT_MUTED: Target = Target { lightness: (0.55, 0.74, 1.0), saturation: (0.0, 0.3, 0.4) };
const MUTED: Target = Target { lightness: (0.3, 0.5, 0.7), saturation: (0.0, 0.3, 0.4) };
const DARK_MUTED: Target = Target { lightness: (0.0, 0.26, 0.45), saturation: (0.0, 0.3, 0.4) };

#[derive(Default)]
struct Palette {
    light_vibrant: Option<Rgb>,
    vibrant: Option<Rgb>,
    dark_vibrant: Option<Rgb>,
    dark_muted: Option<Rgb>,
    dominant: Option<Rgb>,
}

impl Palette {
    fn from(swatches: &[Swatch]) -> Self {
        let max_population = swatches.iter().map(|s| s.population).max().unwrap_or(1) as f32;
        let mut used: Vec<usize> = Vec::new();
        let mut pick = |target: &Target| -> Option<Rgb> {
            let best = swatches
                .iter()
                .enumerate()
                .filter(|(i, s)| {
                    !used.contains(i)
                        && s.hsl[1] >= target.saturation.0
                        && s.hsl[1] <= target.saturation.2
                        && s.hsl[2] >= target.lightness.0
                        && s.hsl[2] <= target.lightness.2
                })
                .map(|(i, s)| {
                    let score = 0.24 * (1.0 - (s.hsl[1] - target.saturation.1).abs())
                        + 0.52 * (1.0 - (s.hsl[2] - target.lightness.1).abs())
                        + 0.24 * (s.population as f32 / max_population);
                    (i, score)
                })
                .max_by(|a, b| a.1.total_cmp(&b.1))?;
            used.push(best.0);
            Some(swatches[best.0].rgb)
        };
        // Palette's order: the vibrant targets claim their swatches first.
        let light_vibrant = pick(&LIGHT_VIBRANT);
        let vibrant = pick(&VIBRANT);
        let dark_vibrant = pick(&DARK_VIBRANT);
        let _light_muted = pick(&LIGHT_MUTED);
        let _muted = pick(&MUTED);
        let dark_muted = pick(&DARK_MUTED);
        let dominant = swatches.iter().max_by_key(|s| s.population).map(|s| s.rgb);
        Palette { light_vibrant, vibrant, dark_vibrant, dark_muted, dominant }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(colors: &[(Rgb, usize)]) -> Vec<[u8; 4]> {
        colors.iter().flat_map(|(c, n)| std::iter::repeat_n([c[0], c[1], c[2], 255], *n)).collect()
    }

    #[test]
    fn a_vivid_cover_gives_its_vivid_colour_as_the_accent() {
        // Mostly a dark muted blue, with a saturated green. (Not orange: hue 10-37
        // is Palette's "red-I line", left out as skin tone, here as in AndroidX.)
        let pixels = image(&[([30, 40, 60], 800), ([40, 200, 90], 200)]);
        let (accent, shade) = art_colors(&pixels).unwrap();
        assert!(accent[1] > 150 && accent[0] < 100, "{accent:?}");
        // The shade is the dark muted blue (quantised to 5 bits) pressed toward black.
        assert!(shade[2] > shade[0] && luminance(shade) < 0.02, "{shade:?}");
    }

    #[test]
    fn a_skin_tone_is_left_out_as_palette_does() {
        let pixels = image(&[([30, 40, 60], 800), ([230, 120, 30], 200)]);
        let (accent, _) = art_colors(&pixels).unwrap();
        assert!(accent[0] < 200, "{accent:?}");
    }

    #[test]
    fn a_dark_accent_is_lifted_until_it_reads_on_a_dark_screen() {
        let lifted = readable_accent([20, 20, 120]);
        assert!(luminance(lifted) >= 0.3 || lifted != [20, 20, 120]);
        assert!(luminance(lifted) > luminance([20, 20, 120]));
    }

    #[test]
    fn text_on_an_accent_is_dark_on_light_and_light_on_dark() {
        assert_eq!(on_accent([250, 230, 120]), [0x11, 0x11, 0x11]);
        assert_eq!(on_accent([60, 30, 120]), [255, 255, 255]);
    }

    #[test]
    fn a_black_and_white_image_has_no_colours() {
        assert!(art_colors(&image(&[([0, 0, 0], 100), ([255, 255, 255], 100)])).is_none());
    }
}
