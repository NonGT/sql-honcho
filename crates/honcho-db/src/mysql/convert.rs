use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::Value as MyValue;

use crate::{Column, Value};

const BINARY_CHARSET: u16 = 63;

pub fn column(c: &mysql_async::Column) -> Column {
    Column {
        name: c.name_str().into_owned(),
        type_name: type_name(c),
        nullable: Some(!c.flags().contains(ColumnFlags::NOT_NULL_FLAG)),
        table: Some(c.org_table_str().into_owned()).filter(|t| !t.is_empty()),
    }
}

fn type_name(c: &mysql_async::Column) -> String {
    use ColumnType::*;
    let binary = c.character_set() == BINARY_CHARSET;
    let flags = c.flags();
    let name = match c.column_type() {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_NULL => "null",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        _ if flags.contains(ColumnFlags::ENUM_FLAG) => "enum",
        _ if flags.contains(ColumnFlags::SET_FLAG) => "set",
        MYSQL_TYPE_STRING if binary => "binary",
        MYSQL_TYPE_STRING => "char",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING if binary => "varbinary",
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => "varchar",
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB
            if binary =>
        {
            "blob"
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            "text"
        }
        _ => "unknown",
    };
    if flags.contains(ColumnFlags::UNSIGNED_FLAG) && is_number(c.column_type()) {
        format!("{name} unsigned")
    } else {
        name.to_string()
    }
}

fn is_number(t: ColumnType) -> bool {
    use ColumnType::*;
    matches!(
        t,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
    )
}

/// Queries use the text protocol, so apart from NULL every cell arrives as
/// the bytes the server printed, and the column type says what they are.
pub fn value(c: &mysql_async::Column, v: MyValue) -> Value {
    use ColumnType::*;
    let bytes = match v {
        MyValue::NULL => return Value::Null,
        MyValue::Bytes(b) => b,
        // only the binary protocol sends these
        MyValue::Int(i) => return Value::Int(i),
        MyValue::UInt(u) => return Value::UInt(u),
        MyValue::Float(f) => return Value::Float(f.into()),
        MyValue::Double(f) => return Value::Float(f),
        other => return Value::Other(other.as_sql(true).trim_matches('\'').to_string()),
    };
    let text = |b: Vec<u8>| String::from_utf8(b).unwrap_or_else(|e| lossy(e.as_bytes()));
    match c.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            let s = text(bytes);
            let parsed = if c.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
                s.parse().ok().map(Value::UInt)
            } else {
                s.parse().ok().map(Value::Int)
            };
            parsed.unwrap_or(Value::Other(s))
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text(bytes);
            s.parse().map(Value::Float).unwrap_or(Value::Other(s))
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Value::Decimal(text(bytes)),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Value::Date(text(bytes)),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Value::Time(text(bytes)),
        MYSQL_TYPE_DATETIME
        | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP
        | MYSQL_TYPE_TIMESTAMP2 => Value::DateTime(text(bytes)),
        MYSQL_TYPE_JSON => Value::Json(text(bytes)),
        MYSQL_TYPE_BIT if bytes.len() <= 8 => {
            Value::UInt(bytes.iter().fold(0, |n, &b| (n << 8) | u64::from(b)))
        }
        MYSQL_TYPE_GEOMETRY => match geometry(&bytes) {
            Some(wkt) => Value::Other(wkt),
            None => Value::Bytes(bytes),
        },
        _ if c.character_set() == BINARY_CHARSET => Value::Bytes(bytes),
        _ => Value::Text(text(bytes)),
    }
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// MySQL geometry is a 4-byte SRID followed by WKB. Shown as WKT, like
/// `ST_AsText` would.
pub fn geometry(b: &[u8]) -> Option<String> {
    let mut r = Wkb {
        b: b.get(4..)?,
        le: true,
    };
    let mut out = String::new();
    r.geometry(&mut out)?;
    r.b.is_empty().then_some(out)
}

struct Wkb<'a> {
    b: &'a [u8],
    le: bool,
}

impl Wkb<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.b.split_at_checked(N)?;
        self.b = rest;
        head.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        let a = self.take()?;
        Some(if self.le {
            u32::from_le_bytes(a)
        } else {
            u32::from_be_bytes(a)
        })
    }

    fn f64(&mut self) -> Option<f64> {
        let a = self.take()?;
        Some(if self.le {
            f64::from_le_bytes(a)
        } else {
            f64::from_be_bytes(a)
        })
    }

    /// Reads a geometry header and returns its type.
    fn header(&mut self) -> Option<u32> {
        let [order] = self.take()?;
        self.le = order == 1;
        self.u32()
    }

    fn geometry(&mut self, out: &mut String) -> Option<()> {
        let kind = self.header()?;
        out.push_str(match kind {
            1 => "POINT",
            2 => "LINESTRING",
            3 => "POLYGON",
            4 => "MULTIPOINT",
            5 => "MULTILINESTRING",
            6 => "MULTIPOLYGON",
            7 => "GEOMETRYCOLLECTION",
            _ => return None,
        });
        self.body(kind, out)
    }

    fn body(&mut self, kind: u32, out: &mut String) -> Option<()> {
        out.push('(');
        match kind {
            1 => {
                let (x, y) = (self.f64()?, self.f64()?);
                out.push_str(&format!("{x} {y}"));
            }
            2 => self.list(out, |r, out| {
                let (x, y) = (r.f64()?, r.f64()?);
                out.push_str(&format!("{x} {y}"));
                Some(())
            })?,
            3 => self.list(out, |r, out| r.body(2, out))?,
            4..=6 => self.list(out, |r, out| {
                let inner = r.header()?;
                r.body(inner, out)
            })?,
            _ => self.list(out, |r, out| r.geometry(out))?,
        }
        out.push(')');
        Some(())
    }

    fn list(
        &mut self,
        out: &mut String,
        mut item: impl FnMut(&mut Self, &mut String) -> Option<()>,
    ) -> Option<()> {
        for i in 0..self.u32()? {
            if i > 0 {
                out.push(',');
            }
            item(self, out)?;
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: f64, y: f64) -> Vec<u8> {
        let mut b = vec![1, 1, 0, 0, 0];
        b.extend(x.to_le_bytes());
        b.extend(y.to_le_bytes());
        b
    }

    #[test]
    fn geometry_as_wkt() {
        let srid = [0u8; 4];
        let p = [&srid[..], &point(1.0, 2.5)].concat();
        assert_eq!(geometry(&p).as_deref(), Some("POINT(1 2.5)"));

        let mut line = vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 2, 0, 0, 0];
        for v in [0.0f64, 0.0, 3.0, 4.0] {
            line.extend(v.to_le_bytes());
        }
        assert_eq!(geometry(&line).as_deref(), Some("LINESTRING(0 0,3 4)"));

        let mut multi = vec![0, 0, 0, 0, 1, 4, 0, 0, 0, 2, 0, 0, 0];
        multi.extend(point(1.0, 1.0));
        multi.extend(point(-2.0, 0.5));
        assert_eq!(
            geometry(&multi).as_deref(),
            Some("MULTIPOINT((1 1),(-2 0.5))")
        );

        let mut coll = vec![0, 0, 0, 0, 1, 7, 0, 0, 0, 1, 0, 0, 0];
        coll.extend(point(5.0, 6.0));
        assert_eq!(
            geometry(&coll).as_deref(),
            Some("GEOMETRYCOLLECTION(POINT(5 6))")
        );

        // big-endian square polygon
        let mut poly = vec![0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 3];
        for v in [0.0f64, 0.0, 1.0, 0.0, 0.0, 0.0] {
            poly.extend(v.to_be_bytes());
        }
        assert_eq!(geometry(&poly).as_deref(), Some("POLYGON((0 0,1 0,0 0))"));
    }

    #[test]
    fn bad_geometry_is_none() {
        assert_eq!(geometry(&[]), None);
        assert_eq!(geometry(&[0, 0, 0, 0, 1, 1, 0, 0, 0, 1]), None);
        assert_eq!(geometry(&[0, 0, 0, 0, 1, 9, 0, 0, 0]), None);
        // a count far larger than the data
        assert_eq!(
            geometry(&[0, 0, 0, 0, 1, 2, 0, 0, 0, 255, 255, 255, 255]),
            None
        );
    }
}
