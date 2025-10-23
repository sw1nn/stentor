use std::collections::HashMap;

pub struct Palette {
    colors: HashMap<String, String>,
}

impl Palette {
    pub fn new(base_bg: &str) -> Self {
        let mut colors = HashMap::new();

        // Generate tinted colors based on the base background
        colors.insert("red".to_string(), tint_color(base_bg, 16, 0, 0));
        colors.insert("green".to_string(), tint_color(base_bg, 0, 16, 0));
        colors.insert("yellow".to_string(), tint_color(base_bg, 16, 12, 0));
        colors.insert("blue".to_string(), tint_color(base_bg, 0, 4, 16));
        colors.insert("magenta".to_string(), tint_color(base_bg, 16, 0, 16));
        colors.insert("cyan".to_string(), tint_color(base_bg, 0, 12, 16));
        colors.insert("orange".to_string(), tint_color(base_bg, 16, 6, 0));
        colors.insert("purple".to_string(), tint_color(base_bg, 6, 0, 16));

        Self { colors }
    }

    /// Get color by slot number (1-4), returns color name and hex value
    pub fn get_slot_color(&self, slot: usize) -> Option<(&str, &str)> {
        let color_names = ["red", "green", "yellow", "blue"];
        if slot == 0 || slot > 4 {
            return None;
        }
        let name = color_names[slot - 1];
        self.colors.get(name).map(|hex| (name, hex.as_str()))
    }
}

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

fn tint_color(base_hex: &str, r_add: i16, g_add: i16, b_add: i16) -> String {
    let (r, g, b) = hex_to_rgb(base_hex).unwrap_or((30, 30, 46));

    let r_new = (r as i16 + r_add).clamp(0, 255) as u8;
    let g_new = (g as i16 + g_add).clamp(0, 255) as u8;
    let b_new = (b as i16 + b_add).clamp(0, 255) as u8;

    format!("#{:02x}{:02x}{:02x}", r_new, g_new, b_new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_palette_colors() {
        let palette = Palette::new("#1e1e2e");

        assert!(palette.get_slot_color(1).is_some()); // red
        assert!(palette.get_slot_color(2).is_some()); // green
        assert!(palette.get_slot_color(3).is_some()); // yellow
        assert!(palette.get_slot_color(4).is_some()); // blue
    }

    #[test]
    fn test_slot_colors() {
        let palette = Palette::new("#1e1e2e");

        let (name, _hex) = palette.get_slot_color(1).unwrap();
        assert_eq!(name, "red");

        let (name, _hex) = palette.get_slot_color(2).unwrap();
        assert_eq!(name, "green");

        let (name, _hex) = palette.get_slot_color(3).unwrap();
        assert_eq!(name, "yellow");

        let (name, _hex) = palette.get_slot_color(4).unwrap();
        assert_eq!(name, "blue");
    }
}
