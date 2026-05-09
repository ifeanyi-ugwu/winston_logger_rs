use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde_json::{Number, Value};
use std::{fmt, sync::Arc};

#[derive(Clone)]
pub enum QueryValue {
    String(String),
    Number(Number),
    Boolean(bool),
    Array(Vec<QueryValue>),
    Regex(Regex),
    DateTime(DateTime<Utc>),
    Duration(Duration),
    Null,
    Function(Arc<dyn Fn(&Value) -> bool + Send + Sync>),
}

impl fmt::Debug for QueryValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryValue::String(s) => write!(f, "String({:?})", s),
            QueryValue::Number(n) => write!(f, "Number({:?})", n),
            QueryValue::Boolean(b) => write!(f, "Boolean({:?})", b),
            QueryValue::Array(a) => write!(f, "Array({:?})", a),
            QueryValue::Regex(r) => write!(f, "Regex({:?})", r),
            QueryValue::DateTime(dt) => write!(f, "DateTime({:?})", dt),
            QueryValue::Duration(d) => write!(f, "Duration({:?})", d),
            QueryValue::Null => write!(f, "Null"),
            QueryValue::Function(_) => {
                write!(f, "Function(Arc<dyn Fn(&Value) -> bool + Send + Sync>)")
            }
        }
    }
}

impl From<&str> for QueryValue {
    fn from(value: &str) -> Self {
        QueryValue::String(value.to_string())
    }
}

impl From<bool> for QueryValue {
    fn from(value: bool) -> Self {
        QueryValue::Boolean(value)
    }
}

impl From<String> for QueryValue {
    fn from(value: String) -> Self {
        QueryValue::String(value)
    }
}

impl From<i32> for QueryValue {
    fn from(value: i32) -> Self {
        QueryValue::Number(Number::from(value))
    }
}

impl From<i64> for QueryValue {
    fn from(value: i64) -> Self {
        QueryValue::Number(Number::from(value))
    }
}

impl From<u32> for QueryValue {
    fn from(value: u32) -> Self {
        QueryValue::Number(Number::from(value))
    }
}

impl From<u64> for QueryValue {
    fn from(value: u64) -> Self {
        QueryValue::Number(Number::from(value))
    }
}

impl From<f32> for QueryValue {
    fn from(value: f32) -> Self {
        QueryValue::Number(Number::from_f64(value as f64).unwrap_or_else(|| Number::from(0)))
    }
}

impl From<f64> for QueryValue {
    fn from(value: f64) -> Self {
        QueryValue::Number(Number::from_f64(value).unwrap_or_else(|| Number::from(0)))
    }
}

impl From<Number> for QueryValue {
    fn from(value: Number) -> Self {
        QueryValue::Number(value)
    }
}

impl From<Regex> for QueryValue {
    fn from(value: Regex) -> Self {
        QueryValue::Regex(value)
    }
}

impl From<DateTime<Utc>> for QueryValue {
    fn from(value: DateTime<Utc>) -> Self {
        QueryValue::DateTime(value)
    }
}

impl From<Duration> for QueryValue {
    fn from(value: Duration) -> Self {
        QueryValue::Duration(value)
    }
}

impl From<Value> for QueryValue {
    fn from(value: Value) -> Self {
        match value {
            Value::String(s) => QueryValue::String(s),
            Value::Number(n) => QueryValue::Number(n),
            Value::Bool(b) => QueryValue::Boolean(b),
            Value::Null => QueryValue::Null,
            Value::Array(arr) => QueryValue::Array(arr.into_iter().map(|v| v.into()).collect()),
            Value::Object(_) => {
                // Objects aren't directly convertible; you might want to handle this case.
                // For now, we're just returning Null, but this could be improved.
                QueryValue::Null
            }
        }
    }
}

// This one might be redundant depending on the context
impl<T: Into<QueryValue>> From<Vec<T>> for QueryValue {
    fn from(vec: Vec<T>) -> Self {
        QueryValue::Array(vec.into_iter().map(Into::into).collect())
    }
}
