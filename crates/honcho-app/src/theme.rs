use gpui::{px, rgb, rgba, App, Global, Hsla, Pixels, SharedString};
use honcho_core::sql::TokenKind;

use crate::keymap::Platform;

pub const FONT_SIZE: f32 = 13.0;

/// The monospace font used everywhere, resolved once at startup.
pub struct EditorFont {
    pub family: SharedString,
    pub size: Pixels,
}

impl Global for EditorFont {}

fn mono_candidates(p: Platform) -> &'static [&'static str] {
    match p {
        Platform::Mac => &["Menlo", "SF Mono", "Monaco"],
        Platform::Windows => &["Cascadia Mono", "Consolas", "Courier New"],
        Platform::Linux => &[
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Noto Sans Mono",
            "Ubuntu Mono",
            "monospace",
        ],
    }
}

/// First preferred font that is installed, else the last candidate (a
/// generic name the platform resolves itself).
pub fn pick_mono_font(installed: &[String], p: Platform) -> &'static str {
    let candidates = mono_candidates(p);
    candidates
        .iter()
        .find(|c| installed.iter().any(|f| f.eq_ignore_ascii_case(c)))
        .unwrap_or(candidates.last().unwrap())
}

pub fn init_font(cx: &mut App) {
    let installed = cx.text_system().all_font_names();
    let family = pick_mono_font(&installed, Platform::current());
    cx.set_global(EditorFont {
        family: family.into(),
        size: px(FONT_SIZE),
    });
}

pub fn font_family(cx: &App) -> SharedString {
    cx.global::<EditorFont>().family.clone()
}
pub const LINE_HEIGHT_RATIO: f32 = 1.55;

pub fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn ca(hex: u32) -> Hsla {
    rgba(hex).into()
}

pub struct Theme;

impl Theme {
    pub fn bg() -> Hsla {
        c(0x1e1f22)
    }
    pub fn panel() -> Hsla {
        c(0x18191b)
    }
    pub fn border() -> Hsla {
        c(0x2b2d30)
    }
    pub fn text() -> Hsla {
        c(0xd4d4d4)
    }
    pub fn muted() -> Hsla {
        c(0x7c7f86)
    }
    pub fn gutter_text() -> Hsla {
        c(0x5a5d63)
    }
    pub fn gutter_active() -> Hsla {
        c(0xc8cad0)
    }
    pub fn current_line() -> Hsla {
        ca(0xffffff0a)
    }
    pub fn selection() -> Hsla {
        ca(0x264f78cc)
    }
    pub fn selection_match() -> Hsla {
        ca(0x5a5d6366)
    }
    pub fn find_match() -> Hsla {
        ca(0x9e6a03aa)
    }
    pub fn input_bg() -> Hsla {
        c(0x2b2d30)
    }
    pub fn error() -> Hsla {
        c(0xf48771)
    }
    pub fn accent_dim() -> Hsla {
        ca(0x3d8bfd33)
    }
    pub fn cursor() -> Hsla {
        c(0xaeafad)
    }
    pub fn accent() -> Hsla {
        c(0x3d8bfd)
    }
    pub fn tab_active() -> Hsla {
        c(0x1e1f22)
    }

    pub fn token(kind: TokenKind) -> Hsla {
        match kind {
            TokenKind::Keyword => c(0x569cd6),
            TokenKind::Type => c(0x4ec9b0),
            TokenKind::Function => c(0xdcdcaa),
            TokenKind::Identifier => c(0xd4d4d4),
            TokenKind::QuotedIdent => c(0x9cdcfe),
            TokenKind::String => c(0xce9178),
            TokenKind::Number => c(0xb5cea8),
            TokenKind::Comment => c(0x6a9955),
            TokenKind::Operator => c(0xd4d4d4),
            TokenKind::Punctuation => c(0xa0a3a8),
            TokenKind::Variable => c(0xc586c0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_installed_font() {
        let installed = vec!["Arial".to_string(), "consolas".to_string()];
        assert_eq!(pick_mono_font(&installed, Platform::Windows), "Consolas");
        assert_eq!(pick_mono_font(&[], Platform::Linux), "monospace");
        assert_eq!(pick_mono_font(&["Menlo".into()], Platform::Mac), "Menlo");
    }
}
