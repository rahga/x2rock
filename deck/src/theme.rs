//! The six colour themes (the TV app's Theme.kt), by the names its settings
//! store: DEFAULT, OCEAN, EMBER, FOREST, ORCHID, ARTWORK.

use slint::Color;

use crate::Palette;
use crate::palette::{Rgb, on_accent};

pub const THEMES: [&str; 6] = ["DEFAULT", "OCEAN", "EMBER", "FOREST", "ORCHID", "ARTWORK"];

pub fn display_name(theme: &str) -> &'static str {
    match theme {
        "OCEAN" => "Ocean",
        "EMBER" => "Ember",
        "FOREST" => "Forest",
        "ORCHID" => "Orchid",
        "ARTWORK" => "Artwork",
        _ => "Default",
    }
}

/// The swatch the theme row shows.
pub fn swatch(theme: &str) -> Color {
    hex(match theme {
        "OCEAN" => 0x4FC3F7,
        "EMBER" => 0xFF7043,
        "FOREST" => 0x66BB6A,
        "ORCHID" => 0xCE93D8,
        "ARTWORK" => 0xE6A15A,
        _ => 0x6650A4,
    })
}

fn hex(rgb: u32) -> Color {
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// tv-material3's dark baseline, which every theme starts from: the TV app's
/// Default is the wallpaper's dynamic scheme on Android 12 and later, and this
/// before it.
fn baseline() -> Palette {
    Palette {
        primary: hex(0xD0BCFF),
        on_primary: hex(0x381E72),
        primary_container: hex(0x4F378B),
        on_primary_container: hex(0xEADDFF),
        secondary: hex(0xCCC2DC),
        on_secondary: hex(0x332D41),
        background: hex(0x1C1B1F),
        on_background: hex(0xE6E1E5),
        surface: hex(0x1C1B1F),
        on_surface: hex(0xE6E1E5),
        surface_variant: hex(0x49454F),
        on_surface_variant: hex(0xCAC4D0),
        inverse_surface: hex(0xE6E1E5),
        inverse_on_surface: hex(0x313033),
        error: hex(0xF2B8B5),
        on_error: hex(0x601410),
    }
}

/// The ten colours a fixed theme sets; the rest are the baseline's.
fn fixed(c: [u32; 9]) -> Palette {
    Palette {
        primary: hex(c[0]),
        on_primary: hex(c[1]),
        primary_container: hex(c[2]),
        on_primary_container: hex(c[3]),
        background: hex(c[4]),
        on_background: hex(c[5]),
        surface: hex(c[6]),
        on_surface: hex(c[5]),
        surface_variant: hex(c[7]),
        on_surface_variant: hex(c[8]),
        ..baseline()
    }
}

/// The scheme for a theme. `accent` is the selected room's cover accent, which
/// only Artwork uses; until a cover has been read it is the Default.
pub fn palette_for(theme: &str, accent: Option<Rgb>) -> Palette {
    match theme {
        "OCEAN" => fixed([0x4FC3F7, 0x003048, 0x00607A, 0xB3EBFF, 0x0A1929, 0xE3F2FD, 0x0D2137, 0x1A3A50, 0xB3EBFF]),
        "EMBER" => fixed([0xFF7043, 0x3B1100, 0x712600, 0xFFDBCF, 0x1A0A00, 0xFFF3E0, 0x2D1000, 0x4A1E00, 0xFFDBCF]),
        "FOREST" => fixed([0x66BB6A, 0x003910, 0x005320, 0xA9F4B5, 0x061209, 0xE8F5E9, 0x0D2110, 0x1B3A1E, 0xA9F4B5]),
        "ORCHID" => fixed([0xCE93D8, 0x3E0056, 0x5B0080, 0xF2DAFF, 0x120D16, 0xF3E5F5, 0x1E1525, 0x36204A, 0xF2DAFF]),
        "ARTWORK" => match accent {
            Some(a) => {
                let on = on_accent(a);
                Palette {
                    primary: Color::from_rgb_u8(a[0], a[1], a[2]),
                    on_primary: Color::from_rgb_u8(on[0], on[1], on[2]),
                    ..baseline()
                }
            }
            None => baseline(),
        },
        _ => baseline(),
    }
}
