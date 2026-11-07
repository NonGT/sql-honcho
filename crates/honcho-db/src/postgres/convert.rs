//! Values arrive as the text the server formats them in (simple query
//! protocol); the column's type says what kind of value that text is.

use tokio_postgres::types::{Kind, Type};

use crate::{Column, Value};

pub fn columns(cols: &[tokio_postgres::Column]) -> Vec<Column> {
    cols.iter()
        .map(|c| Column::new(c.name(), c.type_().name()))
        .collect()
}

pub fn value(ty: &Type, text: Option<&str>) -> Value {
    let Some(text) = text else {
        return Value::Null;
    };
    let owned = || text.to_string();
    match *ty {
        Type::BOOL => Value::Bool(text == "t"),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => text
            .parse()
            .map_or_else(|_| Value::Other(owned()), Value::Int),
        Type::FLOAT4 | Type::FLOAT8 => text
            .parse()
            .map_or_else(|_| Value::Other(owned()), Value::Float),
        Type::NUMERIC => Value::Decimal(owned()),
        Type::BYTEA => bytea(text).map_or_else(|| Value::Other(owned()), Value::Bytes),
        Type::DATE => Value::Date(owned()),
        Type::TIME | Type::TIMETZ => Value::Time(owned()),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => Value::DateTime(owned()),
        Type::JSON | Type::JSONB => Value::Json(owned()),
        Type::TEXT
        | Type::VARCHAR
        | Type::BPCHAR
        | Type::NAME
        | Type::CHAR
        | Type::XML
        | Type::UNKNOWN => Value::Text(owned()),
        _ if matches!(ty.kind(), Kind::Enum(_)) => Value::Text(owned()),
        // uuid, interval, money, arrays, ranges, network types, ...
        _ => Value::Other(owned()),
    }
}

/// `\x0a0b`, the hex format the server sends by default.
fn bytea(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_text_by_type() {
        assert_eq!(value(&Type::INT4, Some("42")), Value::Int(42));
        assert_eq!(value(&Type::INT4, None), Value::Null);
        assert_eq!(value(&Type::BOOL, Some("f")), Value::Bool(false));
        assert_eq!(value(&Type::FLOAT8, Some("1.5")), Value::Float(1.5));
        assert!(matches!(value(&Type::FLOAT8, Some("NaN")), Value::Float(f) if f.is_nan()));
        assert_eq!(
            value(&Type::NUMERIC, Some("12.50")),
            Value::Decimal("12.50".into())
        );
        assert_eq!(
            value(&Type::BYTEA, Some("\\x0a0B")),
            Value::Bytes(vec![10, 11])
        );
        assert_eq!(
            value(&Type::INT4_ARRAY, Some("{1,2}")),
            Value::Other("{1,2}".into())
        );
        assert_eq!(value(&Type::TEXT, Some("สวัสดี")), Value::Text("สวัสดี".into()));
    }
}
