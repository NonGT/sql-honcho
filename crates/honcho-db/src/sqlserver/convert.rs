use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::{ColumnData, ColumnType, FromSql};

use crate::{Column, Value};

pub fn columns(cols: &[tiberius::Column]) -> Vec<Column> {
    cols.iter()
        .map(|c| Column::new(c.name(), type_name(c.column_type())))
        .collect()
}

fn type_name(t: ColumnType) -> &'static str {
    use ColumnType::*;
    match t {
        Null => "null",
        Bit | Bitn => "bit",
        Int1 => "tinyint",
        Int2 => "smallint",
        Int4 | Intn => "int",
        Int8 => "bigint",
        Float4 => "real",
        Float8 | Floatn => "float",
        Money => "money",
        Money4 => "smallmoney",
        Datetime4 => "smalldatetime",
        Datetime | Datetimen => "datetime",
        Datetime2 => "datetime2",
        DatetimeOffsetn => "datetimeoffset",
        Daten => "date",
        Timen => "time",
        Guid => "uniqueidentifier",
        Decimaln | Numericn => "decimal",
        BigVarBin => "varbinary",
        BigBinary => "binary",
        BigVarChar => "varchar",
        BigChar => "char",
        NVarchar => "nvarchar",
        NChar => "nchar",
        Text => "text",
        NText => "ntext",
        Image => "image",
        Xml => "xml",
        Udt => "udt",
        SSVariant => "sql_variant",
    }
}

pub fn value(ty: ColumnType, data: ColumnData<'static>) -> Value {
    convert(ty, &data).unwrap_or(Value::Null)
}

fn convert(ty: ColumnType, data: &ColumnData<'static>) -> Option<Value> {
    Some(match data {
        ColumnData::U8(v) => Value::Int((*v)?.into()),
        ColumnData::I16(v) => Value::Int((*v)?.into()),
        ColumnData::I32(v) => Value::Int((*v)?.into()),
        ColumnData::I64(v) => Value::Int((*v)?),
        // via the shortest text, so 1.1 stays 1.1 rather than 1.100000023…
        ColumnData::F32(v) => Value::Float((*v)?.to_string().parse().unwrap_or_default()),
        // tiberius decodes money as a float; it only ever has four places
        ColumnData::F64(v) if matches!(ty, ColumnType::Money | ColumnType::Money4) => {
            Value::Decimal(format!("{:.4}", (*v)?))
        }
        ColumnData::F64(v) => Value::Float((*v)?),
        ColumnData::Bit(v) => Value::Bool((*v)?),
        ColumnData::String(v) => Value::Text(v.as_deref()?.to_string()),
        ColumnData::Guid(v) => Value::Other((*v)?.to_string().to_uppercase()),
        ColumnData::Binary(v) => Value::Bytes(v.as_deref()?.to_vec()),
        ColumnData::Numeric(v) => Value::Decimal((*v)?.to_string()),
        ColumnData::Xml(v) => Value::Text(v.as_deref()?.to_string()),
        ColumnData::SmallDateTime(_) => datetime(data, 0)?,
        ColumnData::DateTime(_) => datetime(data, 3)?,
        ColumnData::DateTime2(v) => datetime(data, (*v)?.time().scale())?,
        ColumnData::Date(_) => Value::Date(
            NaiveDate::from_sql(data)
                .ok()??
                .format("%Y-%m-%d")
                .to_string(),
        ),
        ColumnData::Time(v) => {
            let scale = (*v)?.scale();
            let t = NaiveTime::from_sql(data).ok()??;
            Value::Time(format!("{}{}", t.format("%H:%M:%S"), fraction(t, scale)))
        }
        ColumnData::DateTimeOffset(v) => {
            let scale = (*v)?.datetime2().time().scale();
            let dt = DateTime::<FixedOffset>::from_sql(data).ok()??;
            Value::DateTime(format!(
                "{}{} {}",
                dt.format("%Y-%m-%d %H:%M:%S"),
                fraction(dt.time(), scale),
                dt.format("%:z")
            ))
        }
    })
}

fn datetime(data: &ColumnData<'static>, scale: u8) -> Option<Value> {
    let dt = NaiveDateTime::from_sql(data).ok()??;
    Some(Value::DateTime(format!(
        "{}{}",
        dt.format("%Y-%m-%d %H:%M:%S"),
        fraction(dt.time(), scale)
    )))
}

/// Fractional seconds to the column's scale, as SSMS shows them.
fn fraction(t: NaiveTime, scale: u8) -> String {
    use chrono::Timelike;
    if scale == 0 {
        return String::new();
    }
    let digits = format!("{:09}", t.nanosecond().min(999_999_999));
    format!(".{}", &digits[..(scale as usize).min(9)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn converts_plain_values() {
        assert_eq!(
            value(ColumnType::Int4, ColumnData::I32(Some(7))),
            Value::Int(7)
        );
        assert_eq!(value(ColumnType::Int4, ColumnData::I32(None)), Value::Null);
        assert_eq!(
            value(ColumnType::Float4, ColumnData::F32(Some(1.1))),
            Value::Float(1.1)
        );
        assert_eq!(
            value(ColumnType::Money, ColumnData::F64(Some(12.5))),
            Value::Decimal("12.5000".into())
        );
        assert_eq!(
            value(
                ColumnType::NVarchar,
                ColumnData::String(Some(Cow::Borrowed("สวัสดี")))
            ),
            Value::Text("สวัสดี".into())
        );
    }

    #[test]
    fn pads_fractions_to_scale() {
        let t = NaiveTime::from_hms_nano_opt(1, 2, 3, 120_000_000).unwrap();
        assert_eq!(fraction(t, 0), "");
        assert_eq!(fraction(t, 3), ".120");
        assert_eq!(fraction(t, 7), ".1200000");
    }
}
