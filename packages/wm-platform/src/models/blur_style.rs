use serde::{Deserialize, Serialize};

/// Backdrop blur material drawn behind a window.
///
/// Applied via the accent policy
/// (`SetWindowCompositionAttribute` + `ACCENT_POLICY`) — the same
/// mechanism tools like TranslucentTB use. The material is only visible
/// through the window's own translucent pixels (e.g. a terminal with
/// per-pixel background opacity < 1.0); windows that paint fully opaque
/// pixels show no visible change.
///
/// # Platform-specific
///
/// Only has an effect on Windows.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlurStyle {
  /// Acrylic blur-behind (`ACCENT_ENABLE_ACRYLICBLURBEHIND`): richer
  /// frost with a noise texture. Known to be more expensive during live
  /// window drags.
  #[default]
  Acrylic,

  /// Classic blur-behind (`ACCENT_ENABLE_BLURBEHIND`): cheaper, no noise
  /// texture, no drag-lag reputation.
  Blur,
}
