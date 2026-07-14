//! Gamescope's window scale calculation from `steamcompmgr.cpp`.

/// Upscaling placement mode. Discriminants match `GamescopeUpscaleScaler`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum ScaleMode {
    /// Fit while respecting a configurable maximum scale.
    #[default]
    Auto = 0,
    /// Fit, then floor scales larger than one to an integer.
    Integer = 1,
    /// Preserve aspect ratio and fit inside the output.
    Fit = 2,
    /// Preserve aspect ratio and cover the output.
    Fill = 3,
    /// Scale each axis independently.
    Stretch = 4,
}

/// Integer pixel dimensions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Inputs that were global variables in the C++ compositor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleContext {
    /// Virtual size exposed to the game.
    pub nested: Size,
    /// Currently active compositor output size.
    pub current_output: Size,
    /// Maximum scale used only by auto mode.
    pub max_window_scale: f32,
    /// Product of overscan and zoom, applied after scaler selection.
    pub global_scale: f32,
}

impl ScaleContext {
    #[must_use]
    pub const fn new(nested: Size, current_output: Size) -> Self {
        Self {
            nested,
            current_output,
            max_window_scale: f32::MAX,
            global_scale: 1.0,
        }
    }

    /// Port of `calc_scale_factor_scaler` and `calc_scale_factor`.
    ///
    /// # Errors
    ///
    /// Returns a [`ScaleError`] when the source, nested, or current output size
    /// has a zero dimension.
    pub fn calculate(self, mode: ScaleMode, source: Size) -> Result<Scale, ScaleError> {
        if self.nested.is_empty() {
            return Err(ScaleError::EmptyNestedSize);
        }
        if self.current_output.is_empty() {
            return Err(ScaleError::EmptyOutputSize);
        }
        if source.is_empty() {
            return Err(ScaleError::EmptySourceSize);
        }

        let x_output_ratio = self.current_output.width as f32 / self.nested.width as f32;
        let y_output_ratio = self.current_output.height as f32 / self.nested.height as f32;
        let output_scale_ratio = x_output_ratio.min(y_output_ratio);

        let x_ratio = self.nested.width as f32 / source.width as f32;
        let y_ratio = self.nested.height as f32 / source.height as f32;

        let (mut x, mut y) = if mode == ScaleMode::Stretch {
            (x_ratio * x_output_ratio, y_ratio * y_output_ratio)
        } else {
            let common = if mode == ScaleMode::Fill {
                x_ratio.max(y_ratio)
            } else {
                x_ratio.min(y_ratio)
            };
            (common, common)
        };

        if mode != ScaleMode::Stretch {
            if mode == ScaleMode::Auto {
                x = self.max_window_scale.min(x);
                y = self.max_window_scale.min(y);
            }

            x *= output_scale_ratio;
            y *= output_scale_ratio;

            if mode == ScaleMode::Integer && x > 1.0 {
                x = x.floor();
                y = x;
            }
        }

        Ok(Scale {
            x: x * self.global_scale,
            y: y * self.global_scale,
        })
    }
}

/// Scale applied from source pixels to output pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    pub x: f32,
    pub y: f32,
}

/// Invalid scale inputs that C++ assumes have already been filtered by CLI and
/// backend setup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScaleError {
    EmptyNestedSize,
    EmptyOutputSize,
    EmptySourceSize,
}

#[cfg(test)]
mod tests {
    use super::{ScaleContext, ScaleError, ScaleMode, Size};

    fn assert_near(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.000_01,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn fit_and_fill_preserve_aspect_ratio() {
        let context = ScaleContext::new(Size::new(1920, 1080), Size::new(1920, 1080));
        let source = Size::new(1280, 960);

        let fit = context.calculate(ScaleMode::Fit, source).unwrap();
        assert_near(fit.x, 1.125);
        assert_near(fit.y, 1.125);

        let fill = context.calculate(ScaleMode::Fill, source).unwrap();
        assert_near(fill.x, 1.5);
        assert_near(fill.y, 1.5);
    }

    #[test]
    fn stretch_uses_independent_axes() {
        let context = ScaleContext::new(Size::new(1920, 1080), Size::new(3840, 2160));
        let scale = context
            .calculate(ScaleMode::Stretch, Size::new(1280, 960))
            .unwrap();
        assert_near(scale.x, 3.0);
        assert_near(scale.y, 2.25);
    }

    #[test]
    fn integer_mode_only_floors_upscaling() {
        let context = ScaleContext::new(Size::new(1920, 1080), Size::new(1920, 1080));
        let up = context
            .calculate(ScaleMode::Integer, Size::new(700, 400))
            .unwrap();
        assert_near(up.x, 2.0);
        assert_near(up.y, 2.0);

        let down = context
            .calculate(ScaleMode::Integer, Size::new(3840, 2160))
            .unwrap();
        assert_near(down.x, 0.5);
        assert_near(down.y, 0.5);
    }

    #[test]
    fn auto_cap_precedes_physical_output_scale_and_global_scale_follows_it() {
        let mut context = ScaleContext::new(Size::new(1280, 720), Size::new(2560, 1440));
        context.max_window_scale = 1.5;
        context.global_scale = 0.9;
        let scale = context
            .calculate(ScaleMode::Auto, Size::new(640, 360))
            .unwrap();
        assert_near(scale.x, 2.7);
        assert_near(scale.y, 2.7);
    }

    #[test]
    fn rejects_zero_dimensions_at_the_safe_rust_boundary() {
        let context = ScaleContext::new(Size::new(0, 720), Size::new(1920, 1080));
        assert_eq!(
            context.calculate(ScaleMode::Fit, Size::new(1280, 720)),
            Err(ScaleError::EmptyNestedSize)
        );
    }
}
