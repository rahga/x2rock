//! The TV app's canvas on any screen.
//!
//! The TV app is laid out on 960x540 dp, a 1080p television at 2x. Here the
//! window's scale factor is set so that the screen is 960 logical pixels wide,
//! or 540 tall, whichever is tighter: a 16:9 screen gets the TV layout exactly,
//! the Deck's 1280x800 gets 960x600 (the extra 60 to the pane), a 4K monitor
//! gets the same layout at 4x. Every dp number from the TV app is then used as
//! a logical pixel, unchanged.

pub const CANVAS_WIDTH: f32 = 960.0;
pub const CANVAS_HEIGHT: f32 = 540.0;

/// The scale factor that fits the canvas into a window of this physical size.
pub fn factor_for(width: u32, height: u32) -> f32 {
    (width as f32 / CANVAS_WIDTH).min(height as f32 / CANVAS_HEIGHT).max(0.25)
}

/// Say how the canvas fits the window, with X2ROCK_DECK_DEBUG set. The fitting
/// itself is the window's own (app.slint's `zoom`): setting the scale factor
/// instead made winit resize the window to keep its logical size, which shrank
/// it a step at a time down to its minimum.
pub fn fit(window: &slint::Window) {
    let size = window.size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    let want = factor_for(size.width, size.height);
    if std::env::var_os("X2ROCK_DECK_DEBUG").is_some() {
        eprintln!(
            "x2rock-deck: window {}x{} physical, scale {} -> {}, canvas {:.0}x{:.0}",
            size.width,
            size.height,
            window.scale_factor(),
            want,
            size.width as f32 / want,
            size.height as f32 / want
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canvas_fits_each_screen() {
        assert_eq!(factor_for(1920, 1080), 2.0);
        assert_eq!(factor_for(3840, 2160), 4.0);
        // The Deck: 960 wide, 600 tall.
        let deck = factor_for(1280, 800);
        assert!((deck - 4.0 / 3.0).abs() < 1e-4);
        assert!((800.0 / deck - 600.0).abs() < 0.1);
        // A tablet in portrait is held to its width.
        assert_eq!(factor_for(1200, 1920), 1.25);
    }
}
