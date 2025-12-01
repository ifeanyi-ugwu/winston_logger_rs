use super::QueryValue;
use chrono::{DateTime, Datelike, Utc};
use serde_json::Value;

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
    //DurationBetween,
    Function,
}

impl Comparator {
    pub fn compare(&self, field_value: &Value, expected_value: &Option<QueryValue>) -> bool {
        self.evaluate(vec![field_value], expected_value)
    }

    pub fn evaluate<'a>(
        &self,
        field_value: Vec<&'a Value>,
        expected_value: &Option<QueryValue>,
    ) -> bool {
        for val in field_value {
            match (self, expected_value) {
                (Comparator::Equals, Some(expected)) => {
                    if self.compare_values(&val, expected) {
                        return true;
                    } else {
                        println!("failed at `equals` check");
                    }
                }
                (Comparator::NotEquals, Some(expected)) => {
                    if !self.compare_values(&val, expected) {
                        return true;
                    } else {
                        println!("failed at `not_equals` check");
                    }
                }
                (Comparator::GreaterThan, Some(expected)) => {
                    if self.compare_numbers(&val, expected, |a, b| a > b) {
                        return true;
                    } else {
                        println!("failed at `greater_than` check");
                    }
                }
                (Comparator::LessThan, Some(expected)) => {
                    if self.compare_numbers(&val, expected, |a, b| a < b) {
                        return true;
                    } else {
                        println!(
                            "failed at `less_than` check: actual={} expected={:?}",
                            val, expected
                        );
                    }
                }
                (Comparator::GreaterThanOrEqual, Some(expected)) => {
                    if self.compare_numbers(&val, expected, |a, b| a >= b) {
                        return true;
                    } else {
                        println!("failed at `greater_than_or_equal` check");
                    }
                }
                (Comparator::LessThanOrEqual, Some(expected)) => {
                    if self.compare_numbers(&val, expected, |a, b| a <= b) {
                        return true;
                    } else {
                        println!("failed at `less_than_or_equal` check");
                    }
                }
                (Comparator::Exists, None) => {
                    //println!("Exists check: path={:?}, result=true", self.path);
                    return true;
                }
                (Comparator::NotExists, None) => {
                    //println!("NotExists check: path={:?}, result=false", self.path);
                    return false;
                }
                (Comparator::Matches, Some(QueryValue::Regex(expected_regex))) => {
                    if let Value::String(actual_str) = val {
                        if expected_regex.is_match(&actual_str) {
                            return true;
                        } else {
                            println!("failed at `matches` check");
                        }
                    } else {
                        println!("failed at `matches` check");
                    }
                }
                (Comparator::NotMatches, Some(QueryValue::Regex(expected_regex))) => {
                    if let Value::String(actual_str) = val {
                        if !expected_regex.is_match(&actual_str) {
                            return true;
                        } else {
                            println!("failed at `not_matches` check");
                        }
                    } else {
                        println!("failed at `not_matches` check");
                    }
                }
                (Comparator::StartsWith, Some(QueryValue::String(expected_prefix))) => {
                    if let Value::String(actual_str) = val {
                        if actual_str.starts_with(expected_prefix) {
                            return true;
                        } else {
                            println!("failed at `starts_with` check");
                        }
                    } else {
                        println!("failed at `starts_with` check");
                    }
                }
                (Comparator::EndsWith, Some(QueryValue::String(expected_suffix))) => {
                    if let Value::String(actual_str) = val {
                        if actual_str.ends_with(expected_suffix) {
                            return true;
                        } else {
                            println!("failed at `ends_with` check");
                        }
                    } else {
                        println!("failed at `ends_with` check");
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
                        println!(
                "failed at `contains` check: none of the array elements contain substring '{}'",
                expected_substring
            );
                    }
                    Value::String(actual_str) => {
                        if actual_str.contains(expected_substring) {
                            return true;
                        } else {
                            println!(
                    "failed at `contains` check: actual string '{}' does not contain substring '{}'",
                    actual_str, expected_substring
                );
                        }
                    }
                    other_value => {
                        println!(
                "failed at `contains` check: expected string or array of strings, found {:?}",
                other_value
            );
                    }
                },
                (Comparator::NotContains, Some(QueryValue::String(expected_substring))) => {
                    if let Value::String(actual_str) = val {
                        if !actual_str.contains(expected_substring) {
                            return true;
                        } else {
                            println!("failed at `not_contains` check");
                        }
                    } else {
                        println!("failed at `not_contains` check");
                    }
                }
                (Comparator::In, Some(QueryValue::Array(expected_array))) => {
                    for expected_val in expected_array {
                        if self.compare_values(&val, expected_val) {
                            return true;
                        } else {
                            println!("failed at `in` check");
                        }
                    }
                }
                (Comparator::NotIn, Some(QueryValue::Array(expected_array))) => {
                    let mut found = false;
                    for expected_val in expected_array {
                        if self.compare_values(&val, expected_val) {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return true;
                    } else {
                        println!("failed at `not_in` check");
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
                    } else {
                        println!("failed at `has_all` check");
                    }
                }
                (Comparator::HasAny, Some(QueryValue::Array(expected_array))) => {
                    if let Value::Array(actual_array) = val {
                        for expected_val in expected_array {
                            for actual_val in actual_array {
                                if self.compare_values(actual_val, expected_val) {
                                    return true;
                                } else {
                                    println!("failed at `has_any` check");
                                }
                            }
                        }
                    } else {
                        println!("failed at `has_any` check");
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
                    } else {
                        println!("failed at `has_none` check");
                    }
                }
                (Comparator::Length, Some(expected_length)) => {
                    if let Value::Array(actual_array) = val {
                        if self.compare_numbers(
                            //&Value::Number(actual_array.len() as f64),
                            &Value::Number(
                                serde_json::Number::from_f64(actual_array.len() as f64)
                                    .unwrap_or(serde_json::Number::from_f64(0.0).unwrap()),
                            ),
                            expected_length,
                            |a, b| a == b,
                        ) {
                            return true;
                        } else {
                            println!("failed at `length` check");
                        }
                    } else {
                        println!("failed at `length` check");
                    }
                }
                (Comparator::Empty, None) => {
                    if let Value::Array(actual_array) = val {
                        if actual_array.is_empty() {
                            return true;
                        } else {
                            println!("failed at `empty` check");
                        }
                    } else {
                        println!("failed at `empty` check");
                    }
                }
                (Comparator::NotEmpty, None) => {
                    if let Value::Array(actual_array) = val {
                        if !actual_array.is_empty() {
                            return true;
                        } else {
                            println!("failed at `not_empty` check");
                        }
                    } else {
                        println!("failed at `not_empty` check");
                    }
                }
                (Comparator::Between, Some(QueryValue::Array(expected_range))) => {
                    if expected_range.len() == 2 {
                        if let (Some(start), Some(end)) =
                            (expected_range.get(0), expected_range.get(1))
                        {
                            if self.compare_numbers(&val, start, |a, b| a >= b)
                                && self.compare_numbers(&val, end, |a, b| a <= b)
                            {
                                return true;
                            } else {
                                println!("failed at `between` check");
                            }
                        } else {
                            println!("failed at `between` check");
                        }
                    } else {
                        println!("failed at `between` check");
                    }
                }
                (Comparator::NotBetween, Some(QueryValue::Array(expected_range))) => {
                    if expected_range.len() == 2 {
                        if let (Some(start), Some(end)) =
                            (expected_range.get(0), expected_range.get(1))
                        {
                            if !(self.compare_numbers(&val, start, |a, b| a >= b)
                                && self.compare_numbers(&val, end, |a, b| a <= b))
                            {
                                return true;
                            } else {
                                println!("failed at `not_between` check");
                            }
                        } else {
                            println!("failed at `not_between` check");
                        }
                    } else {
                        println!("failed at `not_between` check");
                    }
                }
                (Comparator::IsMultipleOf, Some(expected_multiple)) => {
                    if let (Value::Number(actual_num), QueryValue::Number(expected_num)) =
                        (&val, expected_multiple)
                    {
                        if actual_num.as_f64().unwrap_or_default() % expected_num == 0.0 {
                            return true;
                        } else {
                            println!("failed at `is_multiple_of` check");
                        }
                    } else {
                        println!("failed at `is_multiple_of` check");
                    }
                }
                (Comparator::IsDivisibleBy, Some(expected_divisor)) => {
                    if let (Value::Number(actual_num), QueryValue::Number(expected_num)) =
                        (&val, expected_divisor)
                    {
                        if *expected_num != 0.0
                            && actual_num.as_f64().unwrap_or_default() % expected_num == 0.0
                        {
                            return true;
                        } else {
                            println!("failed at `is_divisible_by` check");
                        }
                    } else {
                        println!("failed at `is_divisible_by` check");
                    }
                }
                (Comparator::Before, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(&actual_str) {
                            return actual.with_timezone(&Utc) < *expected;
                        } else {
                            println!("failed at `before` check");
                        }
                    } else {
                        println!("failed at `before` check");
                    }
                }
                (Comparator::After, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(&actual_str) {
                            return actual.with_timezone(&Utc) > *expected;
                        } else {
                            println!("failed at `after` check");
                        }
                    } else {
                        println!("failed at `after` check");
                    }
                }
                (Comparator::SameDay, Some(QueryValue::DateTime(expected))) => {
                    if let Value::String(actual_str) = val {
                        if let Ok(actual) = DateTime::parse_from_rfc3339(&actual_str) {
                            let actual_utc = actual.with_timezone(&Utc);
                            return actual_utc.year() == expected.year()
                                && actual_utc.month() == expected.month()
                                && actual_utc.day() == expected.day();
                        } else {
                            println!("failed at `same_day` check");
                        }
                    } else {
                        println!("failed at `same_day` check");
                    }
                }
                /*(Comparator::DurationBetween, Some(unit, other_field_str, expected_duration)) => {
                    let actual_date_str = match val {
                        Value::String(date_str) => date_str,
                        _ => {
                            println!("failed at `DurationBetween` check: actual_date not a string");
                            return false;
                        }
                    };

                    let actual_date = match DateTime::parse_from_rfc3339(&actual_date_str) {
                        Ok(date) => date.with_timezone(&Utc),
                        Err(_) => {
                            println!("failed at `DurationBetween` check: invalid actual_date");
                            return false;
                        }
                    };

                    let other_field_path =
                        match <FieldPath as std::str::FromStr>::from_str(other_field_str) {
                            Ok(path) => path,
                            Err(_) => {
                                println!(
                                    "failed at `DurationBetween` check: invalid other_field_path"
                                );
                                return false;
                            }
                        };

                    let matching_other_values = other_field_path.extract_refs(value);

                    let other_date_str = match matching_other_values.first() {
                        Some(Value::String(date_str)) => date_str,
                        _ => {
                            println!("failed at `DurationBetween` check: other_date not found or not a string");
                            return false;
                        }
                    };

                    let other_date = match DateTime::parse_from_rfc3339(other_date_str) {
                        Ok(date) => date.with_timezone(&Utc),
                        Err(_) => {
                            println!("failed at `DurationBetween` check: invalid other_date");
                            return false;
                        }
                    };

                    let diff = actual_date.signed_duration_since(other_date).abs();

                    return match unit.as_str() {
                        "days" => diff.num_days() <= expected_duration.num_days(),
                        "hours" => diff.num_hours() <= expected_duration.num_hours(),
                        "minutes" => diff.num_minutes() <= expected_duration.num_minutes(),
                        "seconds" => diff.num_seconds() <= expected_duration.num_seconds(),
                        _ => {
                            println!("failed at `DurationBetween` check: invalid unit");
                            false
                        }
                    };
                }*/
                (Comparator::Function, Some(QueryValue::Function(func))) => {
                    if func(&val) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn compare_values(&self, actual: &Value, expected: &QueryValue) -> bool {
        match (actual, expected) {
            (Value::String(actual_str), QueryValue::String(expected_str)) => {
                actual_str == expected_str
            }
            (Value::Number(actual_num), QueryValue::Number(expected_num)) => {
                actual_num.as_f64().unwrap_or_default() == *expected_num
            }
            (Value::Bool(actual_bool), QueryValue::Boolean(expected_bool)) => {
                actual_bool == expected_bool
            }
            (Value::Array(actual_array), QueryValue::Array(expected_array)) => {
                if actual_array.len() != expected_array.len() {
                    return false;
                }
                for (a, b) in actual_array.iter().zip(expected_array.iter()) {
                    if !self.compare_values(a, b) {
                        return false;
                    }
                }
                true
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

    fn compare_numbers<F>(&self, actual: &Value, expected: &QueryValue, compare_fn: F) -> bool
    where
        F: Fn(f64, f64) -> bool,
    {
        if let (Value::Number(actual_num), QueryValue::Number(expected_num)) = (actual, expected) {
            compare_fn(actual_num.as_f64().unwrap_or_default(), *expected_num)
        } else {
            false
        }
    }
}
