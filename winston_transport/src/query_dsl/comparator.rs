use super::QueryValue;
use chrono::{DateTime, Datelike, Utc};
use serde_json::{Number, Value};
use std::cmp::Ordering;

// Lossless ordering between two serde_json::Numbers. Falls through to f64
// only when the operands aren't both representable as i64 or both as u64,
// which keeps large i64/u64 values exact.
fn cmp_numbers(a: &Number, b: &Number) -> Option<Ordering> {
    if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
        return Some(ai.cmp(&bi));
    }
    if let (Some(au), Some(bu)) = (a.as_u64(), b.as_u64()) {
        return Some(au.cmp(&bu));
    }
    a.as_f64()?.partial_cmp(&b.as_f64()?)
}

fn is_multiple_of_numbers(a: &Number, b: &Number) -> bool {
    if let (Some(ai), Some(bi)) = (a.as_i64(), b.as_i64()) {
        return bi != 0 && ai % bi == 0;
    }
    if let (Some(au), Some(bu)) = (a.as_u64(), b.as_u64()) {
        return bu != 0 && au % bu == 0;
    }
    let (Some(af), Some(bf)) = (a.as_f64(), b.as_f64()) else {
        return false;
    };
    bf != 0.0 && af % bf == 0.0
}

#[derive(Debug, Clone)]
pub enum Comparator {
    Equals,
    NotEquals,
    GreaterThan,
    LessThan,
    GreaterThanOrEqual,
    LessThanOrEqual,
    Exists,
    NotExists,
    Matches,
    NotMatches,
    StartsWith,
    EndsWith,
    Contains,
    NotContains,
    In,
    NotIn,
    HasAll,
    HasAny,
    HasNone,
    Length,
    Empty,
    NotEmpty,
    Between,
    NotBetween,
    IsMultipleOf,
    IsDivisibleBy,
    Before,
    After,
    SameDay,
    Function,
}

impl Comparator {
    pub fn compare(&self, field_value: &Value, expected_value: &Option<QueryValue>) -> bool {
        self.evaluate(vec![field_value], expected_value)
    }

    pub fn evaluate(&self, field_value: Vec<&Value>, expected_value: &Option<QueryValue>) -> bool {
        for val in field_value {
            match (self, expected_value) {
                (Comparator::Equals, Some(expected)) => {
                    if self.compare_values(val, expected) {
                        return true;
                    }
                }
                (Comparator::NotEquals, Some(expected)) => {
                    if !self.compare_values(val, expected) {
                        return true;
                    }
                }
                (Comparator::GreaterThan, Some(expected)) => {
                    if self.compare_numbers(val, expected, Ordering::is_gt) {
                        return true;
                    }
                }
                (Comparator::LessThan, Some(expected)) => {
                    if self.compare_numbers(val, expected, Ordering::is_lt) {
                        return true;
                    }
                }
                (Comparator::GreaterThanOrEqual, Some(expected)) => {
                    if self.compare_numbers(val, expected, Ordering::is_ge) {
                        return true;
                    }
                }
                (Comparator::LessThanOrEqual, Some(expected)) => {
                    if self.compare_numbers(val, expected, Ordering::is_le) {
                        return true;
                    }
                }
                (Comparator::Exists, None) => return true,
                (Comparator::NotExists, None) => return false,
                (Comparator::Matches, Some(QueryValue::Regex(expected_regex))) => {
                    if let Value::String(actual_str) = val {
                        if expected_regex.is_match(actual_str) {
                            return true;
                        }
                    }
                }
                (Comparator::NotMatches, Some(QueryValue::Regex(expected_regex))) => {
                    if let Value::String(actual_str) = val {
                        if !expected_regex.is_match(actual_str) {
                            return true;
                        }
                    }
                }
                (Comparator::StartsWith, Some(QueryValue::String(expected_prefix))) => {
                    if let Value::String(actual_str) = val {
                        if actual_str.starts_with(expected_prefix) {
                            return true;
                        }
                    }
                }
                (Comparator::EndsWith, Some(QueryValue::String(expected_suffix))) => {
                    if let Value::String(actual_str) = val {
                        if actual_str.ends_with(expected_suffix) {
                            return true;
                        }
                    }
                }
                (Comparator::Contains, Some(QueryValue::String(expected_substring))) => match val {
                    Value::Array(array) => {
                        for element in array {
                            if let Value::String(element_str) = element {
                                if element_str.contains(expected_substring) {
                                    return true;
                                }
                            }
                        }
                    }
                    Value::String(actual_str) => {
                        if actual_str.contains(expected_substring) {
                            return true;
                        }
                    }
                    _ => {}
                },
                (Comparator::NotContains, Some(QueryValue::String(expected_substring))) => {
                    if let Value::String(actual_str) = val {
                        if !actual_str.contains(expected_substring) {
                            return true;
                        }
                    }
                }
                (Comparator::In, Some(QueryValue::Array(expected_array))) => {
                    for expected_val in expected_array {
                        if self.compare_values(val, expected_val) {
                            return true;
                        }
                    }
                }
                (Comparator::NotIn, Some(QueryValue::Array(expected_array))) => {
                    let mut found = false;
                    for expected_val in expected_array {
                        if self.compare_values(val, expected_val) {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return true;
                    }
                }
                (Comparator::HasAll, Some(QueryValue::Array(expected_array))) => {
                    if let Value::Array(actual_array) = val {
                        let mut all_found = true;
                        for expected_val in expected_array {
                            let mut found = false;
                            for actual_val in actual_array {
                                if self.compare_values(actual_val, expected_val) {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                all_found = false;
                                break;
                            }
                        }
                        if all_found {
                            return true;
                        }
                    }
                }
                (Comparator::HasAny, Some(QueryValue::Array(expected_array))) => {
                    if let Value::Array(actual_array) = val {
                        for expected_val in expected_array {
                            for actual_val in actual_array {
                                if self.compare_values(actual_val, expected_val) {
                                    return true;
                                }
                            }
                        }
                    }
                }
                (Comparator::HasNone, Some(QueryValue::Array(expected_array))) => {
                    if let Value::Array(actual_array) = val {
                        let mut none_found = true;
                        for expected_val in expected_array {
                            for actual_val in actual_array {
                                if self.compare_values(actual_val, expected_val) {
                                    none_found = false;
                                    break;
                                }
                            }
                            if !none_found {
                                break;
                            }
                        }
                        if none_found {
                            return true;
                        }
                    }
                }
                (Comparator::Length, Some(expected_length)) => {
                    if let Value::Array(actual_array) = val {
                        let len_value = Value::Number(Number::from(actual_array.len() as u64));
                        if self.compare_numbers(&len_value, expected_length, Ordering::is_eq) {
                            return true;
                        }
                    }
                }
                (Comparator::Empty, None) => {
                    if let Value::Array(actual_array) = val {
                        if actual_array.is_empty() {
                            return true;
                        }
                    }
                }
                (Comparator::NotEmpty, None) => {
                    if let Value::Array(actual_array) = val {
                        if !actual_array.is_empty() {
                            return true;
                        }
                    }
                }
                (Comparator::Between, Some(QueryValue::Array(expected_range))) => {
                    if expected_range.len() == 2 {
                        if let (Some(start), Some(end)) =
                            (expected_range.first(), expected_range.get(1))
                        {
                            if self.compare_numbers(val, start, Ordering::is_ge)
                                && self.compare_numbers(val, end, Ordering::is_le)
                            {
                                return true;
                            }
                        }
                    }
                }
                (Comparator::NotBetween, Some(QueryValue::Array(expected_range))) => {
                    if expected_range.len() == 2 {
                        if let (Some(start), Some(end)) =
                            (expected_range.first(), expected_range.get(1))
                        {
                            if !(self.compare_numbers(val, start, Ordering::is_ge)
                                && self.compare_numbers(val, end, Ordering::is_le))
                            {
                                return true;
                            }
                        }
                    }
                }
                (Comparator::IsMultipleOf, Some(QueryValue::Number(expected_num))) => {
                    if let Value::Number(actual_num) = val {
                        if is_multiple_of_numbers(actual_num, expected_num) {
                            return true;
                        }
                    }
                }
                (Comparator::IsDivisibleBy, Some(QueryValue::Number(expected_num))) => {
                    if let Value::Number(actual_num) = val {
                        if is_multiple_of_numbers(actual_num, expected_num) {
                            return true;
                        }
                    }
                }
                (Comparator::Before, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(actual_str) {
                            return actual.with_timezone(&Utc) < *expected;
                        }
                    }
                }
                (Comparator::After, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(actual_str) {
                            return actual.with_timezone(&Utc) > *expected;
                        }
                    }
                }
                (Comparator::SameDay, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(actual_str) {
                            let actual_utc = actual.with_timezone(&Utc);
                            return actual_utc.year() == expected.year()
                                && actual_utc.month() == expected.month()
                                && actual_utc.day() == expected.day();
                        }
                    }
                }
                (Comparator::Function, Some(QueryValue::Function(func))) if func(val) => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    #[allow(clippy::only_used_in_recursion)]
    fn compare_values(&self, actual: &Value, expected: &QueryValue) -> bool {
        match (actual, expected) {
            (Value::String(actual_str), QueryValue::String(expected_str)) => {
                actual_str == expected_str
            }
            (Value::Number(actual_num), QueryValue::Number(expected_num)) => {
                cmp_numbers(actual_num, expected_num).is_some_and(Ordering::is_eq)
            }
            (Value::Bool(actual_bool), QueryValue::Boolean(expected_bool)) => {
                actual_bool == expected_bool
            }
            (Value::Array(actual_array), QueryValue::Array(expected_array)) => {
                actual_array.len() == expected_array.len()
                    && actual_array
                        .iter()
                        .zip(expected_array.iter())
                        .all(|(a, b)| self.compare_values(a, b))
            }
            (Value::String(actual_str), QueryValue::Regex(expected_regex)) => {
                expected_regex.is_match(actual_str)
            }
            (Value::String(actual_str), QueryValue::DateTime(expected_datetime)) => {
                if let Ok(actual_datetime) = DateTime::parse_from_rfc3339(actual_str) {
                    actual_datetime.with_timezone(&Utc) == *expected_datetime
                } else {
                    false
                }
            }
            (Value::Null, QueryValue::Null) => true,
            _ => false,
        }
    }

    fn compare_numbers<F>(&self, actual: &Value, expected: &QueryValue, ord_pred: F) -> bool
    where
        F: FnOnce(Ordering) -> bool,
    {
        let (Value::Number(actual_num), QueryValue::Number(expected_num)) = (actual, expected)
        else {
            return false;
        };
        cmp_numbers(actual_num, expected_num).is_some_and(ord_pred)
    }
}
