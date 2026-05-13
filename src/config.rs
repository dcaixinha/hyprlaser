//! Command-line configuration.
//!
//! Defaults are tuned to roughly match the Google Slides / Excalidraw
//! laser pointer look. All visual knobs are exposed so users can adapt
//! the laser to their preferences.

use clap::Parser;

/// hyprlaser — a Google-Slides-style laser pointer overlay for Hyprland.
#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Laser color. Accepts hex (`#ff2030`, `ff2030`), CSS names
    /// (`red`, `crimson`), or `rgb(...)` / `hsl(...)` syntax.
    #[arg(long, default_value = "#ff2030")]
    pub color: String,

    /// Radius of the bright head dot in pixels. Also the maximum
    /// half-width of the trailing streak.
    #[arg(long, default_value_t = 5.0)]
    pub dot_radius: f32,

    /// Half-width of the streak at the tail tip in pixels. Should be
    /// small (sub-pixel values are fine).
    #[arg(long, default_value_t = 0.5)]
    pub tail_half_width: f32,

    /// How long the motion trail persists, in milliseconds. Older
    /// samples are dropped.
    #[arg(long, default_value_t = 500)]
    pub trail_ms: u64,

    /// Keep the OS cursor visible. By default hyprlaser asks Hyprland
    /// to hide it (`hyprctl keyword cursor:invisible 1`) so only the
    /// laser is shown, and restores it on exit.
    #[arg(long, default_value_t = false)]
    pub keep_cursor: bool,
}

/// Final, validated runtime configuration. Built from `Cli` after
/// parsing color strings etc.
#[derive(Debug, Clone)]
pub struct LaserConfig {
    /// Laser color as **linear** RGB in [0, 1]. The shader expects
    /// linear because we render into an sRGB swapchain (Bgra8UnormSrgb)
    /// which applies the sRGB transfer function on write.
    pub color_linear: [f32; 3],
    pub dot_radius: f32,
    pub tail_half_width: f32,
    pub trail_duration: std::time::Duration,
    pub hide_cursor: bool,
}

impl LaserConfig {
    pub fn from_cli(cli: Cli) -> Result<Self, String> {
        let parsed = csscolorparser::parse(&cli.color)
            .map_err(|e| format!("invalid --color {:?}: {e}", cli.color))?;
        // csscolorparser returns components in [0, 1] sRGB-encoded space.
        // Convert to linear so the shader can multiply by alpha cleanly
        // and the sRGB swapchain re-encodes on write.
        let [r, g, b, _a] = parsed.to_array();
        let color_linear = [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)];

        if cli.dot_radius <= 0.0 || !cli.dot_radius.is_finite() {
            return Err(format!("--dot-radius must be > 0, got {}", cli.dot_radius));
        }
        if cli.tail_half_width < 0.0 || !cli.tail_half_width.is_finite() {
            return Err(format!(
                "--tail-half-width must be >= 0, got {}",
                cli.tail_half_width
            ));
        }
        if cli.trail_ms == 0 {
            return Err("--trail-ms must be > 0".into());
        }

        Ok(Self {
            color_linear,
            dot_radius: cli.dot_radius,
            tail_half_width: cli.tail_half_width,
            trail_duration: std::time::Duration::from_millis(cli.trail_ms),
            hide_cursor: !cli.keep_cursor,
        })
    }
}

/// Standard sRGB EOTF (component-wise). csscolorparser returns sRGB-
/// encoded components in [0, 1]; the shader needs linear values for
/// premultiplied-alpha math to work correctly with the sRGB swapchain.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_to_linear_endpoints() {
        assert!((srgb_to_linear(0.0) - 0.0).abs() < 1e-6);
        assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn srgb_to_linear_midpoint() {
        // 0.5 sRGB ≈ 0.214 linear, well-known check value.
        let v = srgb_to_linear(0.5);
        assert!((v - 0.2140).abs() < 0.01, "got {v}");
    }

    #[test]
    fn parse_hex_color() {
        let cli = Cli {
            color: "#ff0000".into(),
            dot_radius: 5.0,
            tail_half_width: 0.5,
            trail_ms: 500,
            keep_cursor: false,
        };
        let cfg = LaserConfig::from_cli(cli).unwrap();
        assert!((cfg.color_linear[0] - 1.0).abs() < 1e-3);
        assert!(cfg.color_linear[1] < 1e-3);
        assert!(cfg.color_linear[2] < 1e-3);
    }

    #[test]
    fn parse_named_color() {
        let cli = Cli {
            color: "red".into(),
            dot_radius: 5.0,
            tail_half_width: 0.5,
            trail_ms: 500,
            keep_cursor: false,
        };
        let cfg = LaserConfig::from_cli(cli).unwrap();
        assert!((cfg.color_linear[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn rejects_invalid_color() {
        let cli = Cli {
            color: "not-a-color".into(),
            dot_radius: 5.0,
            tail_half_width: 0.5,
            trail_ms: 500,
            keep_cursor: false,
        };
        assert!(LaserConfig::from_cli(cli).is_err());
    }

    #[test]
    fn rejects_zero_radius() {
        let cli = Cli {
            color: "red".into(),
            dot_radius: 0.0,
            tail_half_width: 0.5,
            trail_ms: 500,
            keep_cursor: false,
        };
        assert!(LaserConfig::from_cli(cli).is_err());
    }

    #[test]
    fn rejects_zero_trail() {
        let cli = Cli {
            color: "red".into(),
            dot_radius: 5.0,
            tail_half_width: 0.5,
            trail_ms: 0,
            keep_cursor: false,
        };
        assert!(LaserConfig::from_cli(cli).is_err());
    }
}
