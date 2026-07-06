//! Config schema, matching DESIGN.md §8. Loaded from
//! /home/root/.config/scribe/config.toml on-device; deserializable anywhere.

use serde::Deserialize;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub mode: ModeConfig,
    #[serde(default)]
    pub watch: WatchConfig,
    #[serde(default)]
    pub imagegen: ImageGenConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub hand: HandConfig,
    #[serde(default)]
    pub ink: InkConfig,
    #[serde(default)]
    pub dissolve: DissolveConfig,
    #[serde(default)]
    pub layout: LayoutConfig,
    #[serde(default)]
    pub archive: ArchiveConfig,
    #[serde(default)]
    pub toggle: ToggleConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: ModeConfig::default(),
            watch: WatchConfig::default(),
            imagegen: ImageGenConfig::default(),
            llm: LlmConfig::default(),
            hand: HandConfig::default(),
            ink: InkConfig::default(),
            dissolve: DissolveConfig::default(),
            layout: LayoutConfig::default(),
            archive: ArchiveConfig::default(),
            toggle: ToggleConfig::default(),
        }
    }
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Illustrator,
    Diary,
    Append,
}

/// Which display backend to use. `Xochitl` (default) augments the vendor UI by
/// injecting real pen strokes and erasing via the eraser tool — our original,
/// safe, no-takeover path. `Takeover` stops xochitl and drives the rM2 e-ink
/// panel directly (via rm2fb/SWTCON), rendering our own grayscale surface —
/// enabling true per-pixel fades (like MaximeRivest/riddle on the Paper Pro).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayBackend {
    Xochitl,
    Takeover,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModeConfig {
    #[serde(default = "default_style")]
    pub style: Style,
    #[serde(default = "default_display")]
    pub display: DisplayBackend,
}
fn default_style() -> Style {
    Style::Illustrator
}
fn default_display() -> DisplayBackend {
    DisplayBackend::Xochitl
}
impl Default for ModeConfig {
    fn default() -> Self {
        Self { style: default_style(), display: default_display() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchConfig {
    #[serde(default = "default_dwell_s")]
    pub dwell_s: f64,
    #[serde(default = "default_min_new_ink_px")]
    pub min_new_ink_px: u32,
    #[serde(default = "default_rate_limit_s")]
    pub rate_limit_s: f64,
    #[serde(default = "default_true")]
    pub reset_on_page_change: bool,
}
fn default_dwell_s() -> f64 {
    8.0
}
fn default_min_new_ink_px() -> u32 {
    200
}
fn default_rate_limit_s() -> f64 {
    15.0
}
impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            dwell_s: default_dwell_s(),
            min_new_ink_px: default_min_new_ink_px(),
            rate_limit_s: default_rate_limit_s(),
            reset_on_page_change: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageGenProvider {
    Gemini,
    Openai,
    Fal,
    Openrouter,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageGenConfig {
    #[serde(default = "default_imagegen_provider")]
    pub provider: ImageGenProvider,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_imagegen_style")]
    pub style: String,
    #[serde(default = "default_true")]
    pub caption: bool,
    #[serde(default = "default_max_draw_min")]
    pub max_draw_min: f64,
    #[serde(default)]
    pub refine: bool,
}
fn default_imagegen_provider() -> ImageGenProvider {
    ImageGenProvider::Gemini
}
fn default_imagegen_style() -> String {
    "ink".to_string()
}
fn default_max_draw_min() -> f64 {
    4.0
}
impl Default for ImageGenConfig {
    fn default() -> Self {
        Self {
            provider: default_imagegen_provider(),
            api_key: String::new(),
            style: default_imagegen_style(),
            caption: true,
            max_draw_min: default_max_draw_min(),
            refine: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_session_idle_min")]
    pub session_idle_min: f64,
}
fn default_model() -> String {
    "claude-sonnet-5".to_string()
}
fn default_max_tokens() -> u32 {
    350
}
fn default_session_idle_min() -> f64 {
    15.0
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: default_model(),
            max_tokens: default_max_tokens(),
            session_idle_min: default_session_idle_min(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct HandConfig {
    #[serde(default = "default_x_height_px")]
    pub x_height_px: f32,
    #[serde(default = "default_slant_deg")]
    pub slant_deg: f32,
    #[serde(default)]
    pub seed: u64,
}
fn default_x_height_px() -> f32 {
    28.0
}
fn default_slant_deg() -> f32 {
    3.0
}
impl Default for HandConfig {
    fn default() -> Self {
        Self { x_height_px: default_x_height_px(), slant_deg: default_slant_deg(), seed: 0 }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InkConfig {
    #[serde(default = "default_speed_multiplier")]
    pub speed_multiplier: f32,
}
fn default_speed_multiplier() -> f32 {
    1.0
}
impl Default for InkConfig {
    fn default() -> Self {
        Self { speed_multiplier: default_speed_multiplier() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DissolveConfig {
    #[serde(default = "default_block_px")]
    pub block_px: u32,
    #[serde(default = "default_target_duration_s")]
    pub target_duration_s: f64,
}
fn default_block_px() -> u32 {
    64
}
fn default_target_duration_s() -> f64 {
    6.0
}
impl Default for DissolveConfig {
    fn default() -> Self {
        Self { block_px: default_block_px(), target_duration_s: default_target_duration_s() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Margin {
    #[serde(default = "default_margin_left")]
    pub left: f32,
    #[serde(default = "default_margin_right")]
    pub right: f32,
    #[serde(default = "default_margin_top")]
    pub top: f32,
    #[serde(default = "default_margin_bottom")]
    pub bottom: f32,
}
fn default_margin_left() -> f32 {
    80.0
}
fn default_margin_right() -> f32 {
    60.0
}
fn default_margin_top() -> f32 {
    100.0
}
fn default_margin_bottom() -> f32 {
    60.0
}
impl Default for Margin {
    fn default() -> Self {
        Self {
            left: default_margin_left(),
            right: default_margin_right(),
            top: default_margin_top(),
            bottom: default_margin_bottom(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LayoutConfig {
    #[serde(default)]
    pub margin: Margin,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_keep")]
    pub keep: u32,
}
fn default_keep() -> u32 {
    100
}
impl Default for ArchiveConfig {
    fn default() -> Self {
        Self { enabled: true, keep: default_keep() }
    }
}

/// Zone for the on/off toggle gesture (crate::toggle). Defaults to a box
/// over xochitl's own top-right tool-icon column (verified in
/// device_report.md), so the double-tap feels like it belongs there even
/// though we can't add a real button to xochitl's closed-source UI.
#[derive(Debug, Clone, Deserialize)]
pub struct ToggleConfig {
    #[serde(default = "default_toggle_x")]
    pub x: f32,
    #[serde(default = "default_toggle_y")]
    pub y: f32,
    #[serde(default = "default_toggle_w")]
    pub w: f32,
    #[serde(default = "default_toggle_h")]
    pub h: f32,
    #[serde(default = "default_double_tap_s")]
    pub double_tap_s: f64,
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
}
fn default_toggle_x() -> f32 {
    1250.0
}
fn default_toggle_y() -> f32 {
    0.0
}
fn default_toggle_w() -> f32 {
    154.0
}
fn default_toggle_h() -> f32 {
    200.0
}
fn default_double_tap_s() -> f64 {
    0.4
}
impl Default for ToggleConfig {
    fn default() -> Self {
        Self {
            x: default_toggle_x(),
            y: default_toggle_y(),
            w: default_toggle_w(),
            h: default_toggle_h(),
            double_tap_s: default_double_tap_s(),
            enabled_by_default: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_uses_all_defaults() {
        let cfg = Config::from_toml_str("").unwrap();
        assert_eq!(cfg.mode.style, Style::Illustrator);
        assert_eq!(cfg.watch.dwell_s, 8.0);
        assert_eq!(cfg.dissolve.block_px, 64);
        assert_eq!(cfg.layout.margin.left, 80.0);
    }

    #[test]
    fn partial_toml_overrides_only_given_fields() {
        let cfg = Config::from_toml_str(
            r#"
            [watch]
            dwell_s = 5.0

            [imagegen]
            provider = "openai"
            api_key = "sk-test"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.watch.dwell_s, 5.0);
        assert_eq!(cfg.watch.rate_limit_s, 15.0); // untouched default
        assert_eq!(cfg.imagegen.provider, ImageGenProvider::Openai);
        assert_eq!(cfg.imagegen.api_key, "sk-test");
    }
}
