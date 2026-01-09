//! Generate and cache slot number images for kitty background overlays.
//!
//! Creates PNG images with large centered numerals (1-8) on colored backgrounds
//! that are used as background images in kitty windows to indicate slot assignments.

use ab_glyph::{Font, FontRef, PxScale};
use anyhow::{Context, Result};
use fontconfig::Fontconfig;
use image::{ImageBuffer, Rgba, RgbaImage};
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

/// Image dimensions for slot number overlays (wide aspect ratio to match terminal windows)
const IMAGE_WIDTH: u32 = 1920;
const IMAGE_HEIGHT: u32 = 1080;

/// Font scale for the slot number
const FONT_SCALE: f32 = 400.0;

/// Parse a hex color string to RGB components.
fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    let color_str = if hex.len() == 8 {
        &hex[0..6] // Ignore alpha channel
    } else {
        hex
    };

    if color_str.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&color_str[0..2], 16).ok()?;
    let g = u8::from_str_radix(&color_str[2..4], 16).ok()?;
    let b = u8::from_str_radix(&color_str[4..6], 16).ok()?;

    Some((r, g, b))
}

/// Get a contrasting text color (black or white) for a given background.
fn get_contrast_color(bg_r: u8, bg_g: u8, bg_b: u8) -> (u8, u8, u8) {
    // Perceived brightness calculation
    let brightness = (bg_r as f32 * 0.299 + bg_g as f32 * 0.587 + bg_b as f32 * 0.114) / 255.0;

    if brightness > 0.6 {
        (0, 0, 0) // Black text for light backgrounds
    } else {
        (255, 255, 255) // White text for dark backgrounds
    }
}

/// Load font data using fontconfig pattern.
fn load_font_data(font_pattern: &str) -> Result<Vec<u8>> {
    let fc = Fontconfig::new().context("Failed to initialize fontconfig")?;

    let font = fc
        .find(font_pattern, None)
        .with_context(|| format!("No font found matching pattern: {font_pattern}"))?;

    let font_path = font.path;
    tracing::debug!(path = %font_path.display(), pattern = font_pattern, "Found font");

    std::fs::read(&font_path)
        .with_context(|| format!("Failed to read font file: {}", font_path.display()))
}

/// Ensure slot images exist in the cache directory, generating them if needed.
/// Returns the path to the cache directory containing the images.
pub fn ensure_slot_images(font_pattern: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir()?;

    // Check if all images exist (including empty.png for clearing)
    let all_exist = (1..=8).all(|slot| cache_dir.join(format!("slot_{slot}.png")).exists())
        && cache_dir.join("empty.png").exists();

    if !all_exist {
        tracing::info!(cache_dir = %cache_dir.display(), "Generating slot number images");
        generate_all_slot_images(&cache_dir, font_pattern)?;
    }

    Ok(cache_dir)
}

/// Get the path to the empty image used for clearing backgrounds.
pub fn get_empty_image_path(font_pattern: &str) -> Result<PathBuf> {
    let cache_dir = ensure_slot_images(font_pattern)?;
    Ok(cache_dir.join("empty.png"))
}

/// Get the path to a specific slot image.
pub fn get_slot_image_path(slot_num: usize, font_pattern: &str) -> Result<PathBuf> {
    if !(1..=8).contains(&slot_num) {
        anyhow::bail!("Invalid slot number: {slot_num} (must be 1-8)");
    }

    let cache_dir = ensure_slot_images(font_pattern)?;
    Ok(cache_dir.join(format!("slot_{slot_num}.png")))
}

/// Get the XDG cache directory for slot images.
fn get_cache_dir() -> Result<PathBuf> {
    let xdg_dirs = BaseDirectories::with_prefix("stentor");

    let cache_dir = xdg_dirs
        .create_cache_directory("slot_images")
        .context("Failed to create cache directory")?;

    Ok(cache_dir)
}

/// Generate all 8 slot images plus an empty image for clearing.
fn generate_all_slot_images(cache_dir: &Path, font_pattern: &str) -> Result<()> {
    let font_data = load_font_data(font_pattern)?;
    let font = FontRef::try_from_slice(&font_data).context("Failed to parse font data")?;

    for slot_num in 1..=8 {
        let path = cache_dir.join(format!("slot_{slot_num}.png"));
        generate_slot_image(&font, slot_num, &path)?;
        tracing::debug!(slot_num, path = %path.display(), "Generated slot image");
    }

    // Generate empty image for clearing backgrounds
    let empty_path = cache_dir.join("empty.png");
    generate_empty_image(&empty_path)?;
    tracing::debug!(path = %empty_path.display(), "Generated empty image");

    Ok(())
}

/// Generate an empty transparent image for clearing backgrounds.
fn generate_empty_image(output_path: &Path) -> Result<()> {
    // Small transparent image (1x1 is enough, kitty will scale it)
    let image: RgbaImage = ImageBuffer::from_pixel(1, 1, Rgba([0, 0, 0, 0]));

    image
        .save(output_path)
        .with_context(|| format!("Failed to save empty image to {:?}", output_path))?;

    Ok(())
}

/// Generate a single slot image with the given number (transparent background).
fn generate_slot_image(font: &FontRef<'_>, slot_num: usize, output_path: &Path) -> Result<()> {
    let scale = PxScale::from(FONT_SCALE);
    let digit = slot_num.to_string();

    // Create transparent image
    let mut image: RgbaImage = ImageBuffer::from_pixel(
        IMAGE_WIDTH,
        IMAGE_HEIGHT,
        Rgba([0, 0, 0, 0]), // Fully transparent
    );

    // Calculate glyph metrics for centering
    let glyph = font
        .glyph_id(digit.chars().next().unwrap())
        .with_scale(scale);

    if let Some(outlined) = font.outline_glyph(glyph) {
        let bounds = outlined.px_bounds();
        let glyph_width = bounds.width();
        let glyph_height = bounds.height();

        // Center the glyph on the canvas
        // The draw callback provides (x, y) relative to the glyph's bounding box top-left
        let x_offset = ((IMAGE_WIDTH as f32 - glyph_width) / 2.0) as i32;
        let y_offset = ((IMAGE_HEIGHT as f32 - glyph_height) / 2.0) as i32;

        tracing::trace!(
            glyph_width,
            glyph_height,
            x_offset,
            y_offset,
            bounds_min_x = bounds.min.x,
            bounds_min_y = bounds.min.y,
            "Centering glyph"
        );

        // Draw the glyph with semi-transparent white
        outlined.draw(|x, y, coverage| {
            let px = (x as i32 + x_offset) as u32;
            let py = (y as i32 + y_offset) as u32;

            if px < IMAGE_WIDTH && py < IMAGE_HEIGHT {
                // White color with coverage-based alpha (semi-transparent: 70% max opacity)
                let alpha = (coverage * 0.7 * 255.0) as u8;
                image.put_pixel(px, py, Rgba([255, 255, 255, alpha]));
            }
        });
    }

    image
        .save(output_path)
        .with_context(|| format!("Failed to save slot image to {:?}", output_path))?;

    Ok(())
}

/// Padding from edge for the slot number (in pixels)
const CORNER_PADDING: f32 = 20.0;

/// Generate a slot image with the background color baked in.
/// The image will have the specified background color and a contrasting number in the top-right corner.
pub fn generate_colored_slot_image(
    slot_num: usize,
    bg_color_hex: &str,
    font_pattern: &str,
) -> Result<PathBuf> {
    if !(1..=8).contains(&slot_num) {
        anyhow::bail!("Invalid slot number: {slot_num} (must be 1-8)");
    }

    let (bg_r, bg_g, bg_b) =
        hex_to_rgb(bg_color_hex).with_context(|| format!("Invalid hex color: {bg_color_hex}"))?;

    // Normalize color for filename (lowercase, no #)
    let color_normalized = bg_color_hex.trim_start_matches('#').to_lowercase();
    let cache_dir = get_cache_dir()?;
    // v4 suffix for wide aspect ratio images
    let output_path = cache_dir.join(format!("slot_{slot_num}_{color_normalized}_v4.png"));

    // Check if already cached
    if output_path.exists() {
        tracing::trace!(
            slot_num,
            color = bg_color_hex,
            "Using cached colored slot image"
        );
        return Ok(output_path);
    }

    // Load font and generate
    let font_data = load_font_data(font_pattern)?;
    let font = FontRef::try_from_slice(&font_data).context("Failed to parse font data")?;

    let scale = PxScale::from(FONT_SCALE);
    let digit = slot_num.to_string();

    // Create image with background color
    let mut image: RgbaImage =
        ImageBuffer::from_pixel(IMAGE_WIDTH, IMAGE_HEIGHT, Rgba([bg_r, bg_g, bg_b, 255]));

    // Get contrasting text color
    let (text_r, text_g, text_b) = get_contrast_color(bg_r, bg_g, bg_b);

    // Calculate glyph metrics for top-right positioning
    let glyph = font
        .glyph_id(digit.chars().next().unwrap())
        .with_scale(scale);

    if let Some(outlined) = font.outline_glyph(glyph) {
        let bounds = outlined.px_bounds();
        let glyph_width = bounds.width();

        // Position in top-right corner with padding
        let x_offset = (IMAGE_WIDTH as f32 - glyph_width - CORNER_PADDING) as i32;
        let y_offset = CORNER_PADDING as i32;

        // Draw the glyph with contrasting color, semi-transparent
        outlined.draw(|x, y, coverage| {
            let px = (x as i32 + x_offset) as u32;
            let py = (y as i32 + y_offset) as u32;

            if px < IMAGE_WIDTH && py < IMAGE_HEIGHT {
                // Blend text color with background based on coverage
                let alpha = coverage * 0.7; // 70% max opacity for the number
                let inv_alpha = 1.0 - alpha;

                let r = (text_r as f32 * alpha + bg_r as f32 * inv_alpha) as u8;
                let g = (text_g as f32 * alpha + bg_g as f32 * inv_alpha) as u8;
                let b = (text_b as f32 * alpha + bg_b as f32 * inv_alpha) as u8;

                image.put_pixel(px, py, Rgba([r, g, b, 255]));
            }
        });
    }

    image
        .save(&output_path)
        .with_context(|| format!("Failed to save colored slot image to {:?}", output_path))?;

    tracing::debug!(slot_num, color = bg_color_hex, path = %output_path.display(), "Generated colored slot image");

    Ok(output_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_slot_image_path_valid() {
        // This test requires fontconfig and a system font to be available
        for slot in 1..=8 {
            let result = get_slot_image_path(slot, "monospace:bold");
            assert!(result.is_ok(), "Failed for slot {slot}: {result:?}");
            let path = result.unwrap();
            assert!(path.to_string_lossy().contains(&format!("slot_{slot}.png")));
        }
    }

    #[test]
    fn test_get_slot_image_path_invalid() {
        assert!(get_slot_image_path(0, "monospace:bold").is_err());
        assert!(get_slot_image_path(9, "monospace:bold").is_err());
    }

    #[test]
    fn test_load_font_data() {
        // Should be able to load the default monospace font
        let result = load_font_data("monospace:bold");
        assert!(result.is_ok(), "Failed to load font: {result:?}");
        let data = result.unwrap();
        assert!(!data.is_empty(), "Font data should not be empty");
    }

    #[test]
    fn test_font_loads() {
        let font_data = load_font_data("monospace:bold").unwrap();
        let font = FontRef::try_from_slice(&font_data);
        assert!(font.is_ok(), "Failed to parse font data");
    }

    #[test]
    fn test_generate_colored_slot_image() {
        let result = generate_colored_slot_image(1, "#ff5733", "monospace:bold");
        assert!(
            result.is_ok(),
            "Failed to generate colored slot image: {result:?}"
        );
        let path = result.unwrap();
        assert!(path.exists(), "Generated image should exist at {path:?}");
        assert!(
            path.to_string_lossy().contains("slot_1_ff5733_v4.png"),
            "Path should include color and version: {path:?}"
        );
    }

    #[test]
    fn test_generate_colored_slot_image_caches() {
        // Generate twice, should use cache second time
        let path1 = generate_colored_slot_image(2, "#00ff00", "monospace:bold").unwrap();
        let path2 = generate_colored_slot_image(2, "#00ff00", "monospace:bold").unwrap();
        assert_eq!(path1, path2, "Should return same cached path");
    }

    #[test]
    fn test_generate_colored_slot_image_invalid_slot() {
        assert!(generate_colored_slot_image(0, "#ff5733", "monospace:bold").is_err());
        assert!(generate_colored_slot_image(9, "#ff5733", "monospace:bold").is_err());
    }

    #[test]
    fn test_generate_colored_slot_image_invalid_color() {
        assert!(generate_colored_slot_image(1, "invalid", "monospace:bold").is_err());
        assert!(generate_colored_slot_image(1, "#ff", "monospace:bold").is_err());
    }

    #[test]
    fn test_hex_to_rgb_valid() {
        assert_eq!(hex_to_rgb("#ff5733"), Some((255, 87, 51)));
        assert_eq!(hex_to_rgb("ff5733"), Some((255, 87, 51)));
        assert_eq!(hex_to_rgb("#000000"), Some((0, 0, 0)));
        assert_eq!(hex_to_rgb("#ffffff"), Some((255, 255, 255)));
    }

    #[test]
    fn test_hex_to_rgb_with_alpha() {
        // Should ignore alpha channel
        assert_eq!(hex_to_rgb("#ff5733aa"), Some((255, 87, 51)));
    }

    #[test]
    fn test_get_contrast_color() {
        // Light background -> black text
        assert_eq!(get_contrast_color(255, 255, 255), (0, 0, 0));
        // Dark background -> white text
        assert_eq!(get_contrast_color(0, 0, 0), (255, 255, 255));
    }
}
