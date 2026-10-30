pub mod editor;
pub mod sql;

pub use editor::{Editor, Motion, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
        }
    }
}

/// The editor works on `\n` only; the original ending is restored on save.
pub fn normalize_line_endings(text: &str) -> (String, LineEnding) {
    if text.contains("\r\n") {
        (text.replace("\r\n", "\n"), LineEnding::CrLf)
    } else {
        (text.to_string(), LineEnding::Lf)
    }
}

pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}
