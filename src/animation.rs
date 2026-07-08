use egui::Color32;

#[inline]
pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

const INTRO_DUR_MS: u32 = 300;
const ANCHOR_DUR_MS: u32 = 450;

/// Tracks a timed transition and provides interpolated values.
#[derive(Debug, Clone, Copy)]
pub struct Transition {
    pub start: f64,
    pub dur_ms: u32,
}

impl Transition {
    pub fn new(start: f64, dur_ms: u32) -> Self {
        Self { start, dur_ms }
    }

    pub fn intro(start: f64) -> Self {
        Self::new(start, INTRO_DUR_MS)
    }

    pub fn anchor(start: f64) -> Self {
        Self::new(start, ANCHOR_DUR_MS)
    }

    #[inline]
    pub fn t(&self, now: f64) -> f32 {
        ((now - self.start) as f32 * 1000.0 / self.dur_ms as f32).clamp(0.0, 1.0)
    }

    #[inline]
    pub fn value(&self, now: f64, from: f32, to: f32) -> f32 {
        let t = ease_out_cubic(self.t(now));
        from + (to - from) * t
    }

    #[inline]
    pub fn done(&self, now: f64) -> bool {
        self.t(now) >= 1.0
    }
}

/// Seamless 0..1 triangle wave from phase (period = 2π).
#[inline]
fn wave(phase: f32) -> f32 {
    let t = (phase % (2.0 * std::f32::consts::PI)) / (2.0 * std::f32::consts::PI);
    if t < 0.5 {
        t * 2.0
    } else {
        2.0 - t * 2.0
    }
}

/// Warm gradient background for input box.
/// Subtle warm↔cool breathing, seamlessly looping.
pub fn warm_gradient_bg(phase: f32) -> Color32 {
    let w = wave(phase);
    // Base #F2F0F4, warm shift +6/-4/-10, cool shift -2/+2/+6
    let r = (0xF2_i16 + (w * 8.0 - 4.0) as i16).clamp(0, 255) as u8;
    let g = (0xF0_i16 + (w * 6.0 - 3.0) as i16).clamp(0, 255) as u8;
    let b = (0xF4_i16 + (w * 12.0 - 6.0) as i16).clamp(0, 255) as u8;
    Color32::from_rgb(r, g, b)
}

/// Warm gradient border — gentle hue rotation, seamlessly looping.
pub fn warm_gradient_border(phase: f32) -> Color32 {
    let w = wave(phase * 1.0);
    let w2 = wave(phase * 1.0 + std::f32::consts::PI / 3.0);
    // Soft peach #D8B898 ↔ lavender #C0B0D8 ↔ sage #B0C8B0
    let r = (0xCC_i16 + (w * 16.0 - 8.0) as i16).clamp(0, 255) as u8;
    let g = (0xB0_i16 + (w2 * 14.0 - 7.0) as i16).clamp(0, 255) as u8;
    let b = (0xA8_i16 + (w * 18.0 - 9.0) as i16).clamp(0, 255) as u8;
    Color32::from_rgb(r, g, b)
}

/// Global chat background: very subtle warm↔cool shift.
pub fn warm_gradient_top(phase: f32) -> Color32 {
    let w = wave(phase * 0.5);
    // Ivory #F8F6F2 ↔ mist #F4F5F8
    let r = (0xF6_i16 + (w * 4.0 - 2.0) as i16).clamp(0, 255) as u8;
    let g = (0xF5_i16 + (w * 3.0 - 1.5) as i16).clamp(0, 255) as u8;
    let b = (0xF2_i16 + (w * 8.0 - 4.0) as i16).clamp(0, 255) as u8;
    Color32::from_rgb(r, g, b)
}

/// Interpolate two f32 values with a given t (0..1).
#[inline]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
