use gpui_kit::{Hsla, Rgba};
use libghostty_vt::style::{Palette, RgbColor};

/// An 8-bit sRGB colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// `0xRRGGBB`.
    pub const fn hex(value: u32) -> Self {
        Self {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
    }

    /// Mixes `self` towards `other`; `amount` 0 keeps `self`, 1 gives `other`.
    pub fn mix(self, other: Rgb, amount: f32) -> Rgb {
        let amount = amount.clamp(0.0, 1.0);
        let channel =
            |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
        Rgb {
            r: channel(self.r, other.r),
            g: channel(self.g, other.g),
            b: channel(self.b, other.b),
        }
    }

    pub fn to_hsla(self) -> Hsla {
        Rgba {
            r: f32::from(self.r) / 255.0,
            g: f32::from(self.g) / 255.0,
            b: f32::from(self.b) / 255.0,
            a: 1.0,
        }
        .into()
    }
}

impl From<RgbColor> for Rgb {
    fn from(c: RgbColor) -> Self {
        Rgb {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<Rgb> for RgbColor {
    fn from(c: Rgb) -> Self {
        RgbColor {
            r: c.r,
            g: c.g,
            b: c.b,
        }
    }
}

impl From<Rgb> for Hsla {
    fn from(c: Rgb) -> Self {
        c.to_hsla()
    }
}

/// Terminal colours. Programs can still override them with OSC 4/10/11/12.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalTheme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    /// The 16 ANSI colours: black, red, green, yellow, blue, magenta, cyan,
    /// white, then their bright variants. Indices 16–255 follow xterm.
    pub ansi: [Rgb; 16],
}

impl TerminalTheme {
    /// Ghostty's default colours.
    pub fn dark() -> Self {
        Self {
            foreground: Rgb::hex(0xffffff),
            background: Rgb::hex(0x282c34),
            cursor: Rgb::hex(0xffffff),
            selection: Rgb::hex(0x4a5263),
            ansi: [
                Rgb::hex(0x1d1f21),
                Rgb::hex(0xcc6666),
                Rgb::hex(0xb5bd68),
                Rgb::hex(0xf0c674),
                Rgb::hex(0x81a2be),
                Rgb::hex(0xb294bb),
                Rgb::hex(0x8abeb7),
                Rgb::hex(0xc5c8c6),
                Rgb::hex(0x666666),
                Rgb::hex(0xd54e53),
                Rgb::hex(0xb9ca4a),
                Rgb::hex(0xe7c547),
                Rgb::hex(0x7aa6da),
                Rgb::hex(0xc397d8),
                Rgb::hex(0x70c0b1),
                Rgb::hex(0xeaeaea),
            ],
        }
    }

    /// A light theme with GitHub Light's ANSI colours.
    pub fn light() -> Self {
        Self {
            foreground: Rgb::hex(0x1f2328),
            background: Rgb::hex(0xffffff),
            cursor: Rgb::hex(0x1f2328),
            selection: Rgb::hex(0xb6d6fd),
            ansi: [
                Rgb::hex(0x24292f),
                Rgb::hex(0xcf222e),
                Rgb::hex(0x116329),
                Rgb::hex(0x4d2d00),
                Rgb::hex(0x0969da),
                Rgb::hex(0x8250df),
                Rgb::hex(0x1b7c83),
                Rgb::hex(0x6e7781),
                Rgb::hex(0x57606a),
                Rgb::hex(0xa40e26),
                Rgb::hex(0x1a7f37),
                Rgb::hex(0x633c01),
                Rgb::hex(0x218bff),
                Rgb::hex(0xa475f9),
                Rgb::hex(0x3192aa),
                Rgb::hex(0x8c959f),
            ],
        }
    }

    /// The full 256-colour palette: `ansi`, the 6×6×6 cube, then the grey ramp.
    pub fn palette(&self) -> [Rgb; 256] {
        let mut palette = [Rgb::default(); 256];
        for (index, slot) in palette.iter_mut().enumerate() {
            *slot = match index {
                0..16 => self.ansi[index],
                _ => xterm_color(index as u8),
            };
        }
        palette
    }

    pub(crate) fn ghostty_palette(&self) -> Palette {
        Palette(self.palette().map(RgbColor::from))
    }
}

/// xterm's colour for palette indices 16–255 (cube and grey ramp).
pub(crate) fn xterm_color(index: u8) -> Rgb {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        16..=231 => {
            let i = index - 16;
            Rgb {
                r: LEVELS[usize::from(i / 36)],
                g: LEVELS[usize::from(i / 6 % 6)],
                b: LEVELS[usize::from(i % 6)],
            }
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb {
                r: level,
                g: level,
                b: level,
            }
        }
        _ => Rgb::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_mix() {
        let c = Rgb::hex(0x102030);
        assert_eq!((c.r, c.g, c.b), (0x10, 0x20, 0x30));
        let black = Rgb::hex(0x000000);
        let white = Rgb::hex(0xffffff);
        assert_eq!(black.mix(white, 0.5), Rgb::hex(0x808080));
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 2.0), white);
    }

    #[test]
    fn to_hsla() {
        let red: Hsla = Rgb::hex(0xff0000).into();
        assert!((red.h - 0.0).abs() < 1e-6);
        assert!((red.s - 1.0).abs() < 1e-6);
        assert!((red.l - 0.5).abs() < 1e-6);
        assert_eq!(red.a, 1.0);
        let grey: Hsla = Rgb::hex(0x808080).into();
        assert_eq!(grey.s, 0.0);
    }

    #[test]
    fn xterm_cube_and_ramp() {
        assert_eq!(xterm_color(16), Rgb::hex(0x000000));
        assert_eq!(xterm_color(196), Rgb::hex(0xff0000));
        assert_eq!(xterm_color(231), Rgb::hex(0xffffff));
        assert_eq!(xterm_color(232), Rgb::hex(0x080808));
        assert_eq!(xterm_color(255), Rgb::hex(0xeeeeee));
    }

    #[test]
    fn dark_theme_matches_ghostty_defaults() {
        let ghostty = Palette::default();
        let ours = TerminalTheme::dark().palette();
        for (index, color) in ours.iter().enumerate() {
            assert_eq!(*color, Rgb::from(ghostty.0[index]), "palette index {index}");
        }
    }
}
