use std::borrow::Cow;
use std::fmt::Write;

/// A single cell. Drivers convert server types into one of these so the
/// grid can show any column without knowing the driver.
///
/// Temporal and decimal values are kept as the text the server produced, so
/// nothing is lost to float or timezone conversions.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(String),
    Time(String),
    DateTime(String),
    Json(String),
    /// Anything without a better mapping (spatial, ranges, ...), as text.
    Other(String),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_)
        )
    }

    /// Text for display. Binary is shown as hex, cut at `max_bytes`.
    pub fn display(&self, max_bytes: usize) -> Cow<'_, str> {
        match self {
            Value::Null => Cow::Borrowed("NULL"),
            Value::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Value::Int(n) => Cow::Owned(n.to_string()),
            Value::UInt(n) => Cow::Owned(n.to_string()),
            Value::Float(f) => Cow::Owned(f.to_string()),
            Value::Bytes(b) => {
                let shown = &b[..b.len().min(max_bytes)];
                let mut s = String::with_capacity(2 + shown.len() * 2 + 3);
                s.push_str("0x");
                for byte in shown {
                    let _ = write!(s, "{byte:02X}");
                }
                if b.len() > max_bytes {
                    s.push('…');
                }
                Cow::Owned(s)
            }
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Json(s)
            | Value::Other(s) => Cow::Borrowed(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_values() {
        assert_eq!(Value::Null.display(16), "NULL");
        assert_eq!(Value::Int(-3).display(16), "-3");
        assert_eq!(Value::Decimal("12.50".into()).display(16), "12.50");
        assert_eq!(Value::Bytes(vec![0xde, 0xad, 0xbe]).display(2), "0xDEAD…");
        assert_eq!(Value::Bool(true).display(16), "true");
        assert_eq!(Value::UInt(u64::MAX).display(16), "18446744073709551615");
        assert_eq!(Value::Text("ชื่อ".into()).display(16), "ชื่อ");
        assert_eq!(Value::Bytes(vec![]).display(4), "0x");
        assert!(Value::Null.is_null());
        assert!(Value::Float(1.5).is_numeric());
        assert!(!Value::Text("1".into()).is_numeric());
    }
}
