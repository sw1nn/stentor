//! Generate and cache slot number images for kitty background overlays.
//!
//! Creates transparent PNG images with large centered numerals (1-8) that are
//! used as background images in kitty windows to indicate slot assignments.

use ab_glyph::{Font, FontRef, PxScale};
use anyhow::{Context, Result};
use fontconfig::Fontconfig;
use image::{ImageBuffer, Rgba, RgbaImage};
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

/// Image dimensions for slot number overlays
const IMAGE_SIZE: u32 = 512;

/// Font scale for the slot number
const FONT_SCALE: f32 = 400.0;

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

    // Check if all images exist
    let all_exist = (1..=8).all(|slot| cache_dir.join(format!("slot_{slot}.png")).exists());

    if !all_exist {
        tracing::info!(cache_dir = %cache_dir.display(), "Generating slot number images");
        generate_all_slot_images(&cache_dir, font_pattern)?;
    }

    Ok(cache_dir)
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

/// Generate all 8 slot images.
fn generate_all_slot_images(cache_dir: &Path, font_pattern: &str) -> Result<()> {
    let font_data = load_font_data(font_pattern)?;
    let font = FontRef::try_from_slice(&font_data)
        .context("Failed to parse font data")?;

    for slot_num in 1..=8 {
        let path = cache_dir.join(format!("slot_{slot_num}.png"));
        generate_slot_image(&font, slot_num, &path)?;
        tracing::debug!(slot_num, path = %path.display(), "Generated slot image");
    }
    Ok(())
}

/// Generate a single slot image with the given number.
fn generate_slot_image(font: &FontRef<'_>, slot_num: usize, output_path: &Path) -> Result<()> {
    let scale = PxScale::from(FONT_SCALE);
    let digit = slot_num.to_string();

    // Create transparent image
    let mut image: RgbaImage = ImageBuffer::from_pixel(
        IMAGE_SIZE,
        IMAGE_SIZE,
        Rgba([0, 0, 0, 0]), // Fully transparent
    );

    // Calculate glyph metrics for centering
    let glyph = font.glyph_id(digit.chars().next().unwrap()).with_scale(scale);

    if let Some(outlined) = font.outline_glyph(glyph) {
        let bounds = outlined.px_bounds();
        let glyph_width = bounds.width();
        let glyph_height = bounds.height();

        // Center the glyph on the canvas
        // The draw callback provides (x, y) relative to the glyph's bounding box top-left
        let x_offset = ((IMAGE_SIZE as f32 - glyph_width) / 2.0) as i32;
        let y_offset = ((IMAGE_SIZE as f32 - glyph_height) / 2.0) as i32;

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

            if px < IMAGE_SIZE && py < IMAGE_SIZE {
                // White color with coverage-based alpha (semi-transparent: 70% max opacity)
                let alpha = (coverage * 0.7 * 255.0) as u8;
                image.put_pixel(px, py, Rgba([255, 255, 255, alpha]));
            }
        });
    }

    image.save(output_path)
        .with_context(|| format!("Failed to save slot image to {:?}", output_path))?;

    Ok(())
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
}
