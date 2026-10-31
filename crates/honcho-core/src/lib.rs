pub mod editor;
pub mod search;
pub mod sql;

pub use editor::{Editor, Motion, Selection};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

impl LineEnding {
    /// What a file with no line breaks yet gets: CRLF on Windows, like VS Code.
    pub fn native() -> Self {
        if cfg!(windows) {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        }
    }

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
    } else if text.contains('\n') {
        (text.to_string(), LineEnding::Lf)
    } else {
        (text.to_string(), LineEnding::native())
    }
}

pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings() {
        assert_eq!(
            normalize_line_endings("a\r\nb"),
            ("a\nb".into(), LineEnding::CrLf)
        );
        assert_eq!(normalize_line_endings("a\nb").1, LineEnding::Lf);
        assert_eq!(normalize_line_endings("").1, LineEnding::native());
        assert_eq!(restore_line_endings("a\nb", LineEnding::CrLf), "a\r\nb");
    }
}
