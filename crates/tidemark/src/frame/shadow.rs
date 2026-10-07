//! Pixels for a client decoration shadow. Coordinates are in logical pixels;
//! neither the shadow's margin nor its transparent centre is part of window input.

pub(super) const MARGIN: i32 = 32;

fn distance(x: f32, y: f32, width: f32, height: f32) -> f32 {
    let radius = 12.0_f32.min(width / 2.0).min(height / 2.0);
    let dx = (x - width / 2.0).abs() - (width / 2.0 - radius);
    let dy = (y - height / 2.0).abs() - (height / 2.0 - radius);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius
}

fn alpha(x: f32, y: f32, width: f32, height: f32, active: bool) -> u8 {
    // Leave the client itself clear, including when it uses a translucent background.
    if distance(x, y, width, height) < 0.0 {
        return 0;
    }
    let broad = distance(x, y - 4.0, width, height).max(0.0);
    let contact = distance(x, y, width, height).max(0.0);
    let strength = if active { 1.0 } else { 0.55 };
    let opacity = strength
        * (0.20 * (-broad * broad / 162.0).exp() + 0.10 * (-contact * contact / 8.0).exp());
    (opacity * 255.0).round() as u8
}

pub(super) fn pixels(width: u32, height: u32, scale: i32, active: bool) -> Vec<u8> {
    let buffer_width = (width as usize + 2 * MARGIN as usize) * scale as usize;
    let buffer_height = (height as usize + 2 * MARGIN as usize) * scale as usize;
    let mut pixels = vec![0; buffer_width * buffer_height * 4];
    for y in 0..buffer_height {
        let logical_y = (y as f32 + 0.5) / scale as f32 - MARGIN as f32;
        for x in 0..buffer_width {
            let logical_x = (x as f32 + 0.5) / scale as f32 - MARGIN as f32;
            let a = alpha(logical_x, logical_y, width as f32, height as f32, active);
            // wl_shm ARGB8888 is native endian and premultiplied; black has zero RGB.
            let offset = (y * buffer_width + x) * 4;
            pixels[offset..offset + 4].copy_from_slice(&(u32::from(a) << 24).to_ne_bytes());
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shadow_fades_outside_the_window_without_darkening_its_content() {
        let near = alpha(-1.0, 150.0, 400.0, 300.0, true);
        let far = alpha(-20.0, 150.0, 400.0, 300.0, true);
        assert!(near > far && far > 0);
        assert_eq!(alpha(-32.0, 150.0, 400.0, 300.0, true), 0);
        assert_eq!(alpha(200.0, 150.0, 400.0, 300.0, true), 0);
        // The transparent cutout of a rounded corner can carry a shadow.
        assert!(alpha(0.0, 0.0, 400.0, 300.0, true) > 0);
        assert!(near > alpha(-1.0, 150.0, 400.0, 300.0, false));
    }

    #[test]
    fn scaling_changes_resolution_without_changing_the_logical_shadow() {
        let normal = pixels(80, 60, 1, true);
        let scaled = pixels(80, 60, 2, true);
        let sample = |bytes: &[u8], stride: usize, x: usize, y: usize| {
            let offset = (y * stride + x) * 4;
            u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap()) >> 24
        };
        assert_eq!(scaled.len(), normal.len() * 4);
        assert!(sample(&scaled, 288, 62, 124) > 0);
        assert_eq!(sample(&scaled, 288, 144, 124), 0);
        assert!(sample(&normal, 144, 31, 62).abs_diff(sample(&scaled, 288, 62, 124)) <= 2);
    }
}
