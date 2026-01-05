use chrono::{DateTime, Duration, Utc};
use logform::LogInfo;
use parse_datetime::parse_datetime;
use regex::Regex;
use serde_json::Value;
use std::str::FromStr;

use crate::query_dsl::dlc::alpha::a::QueryNode;
#[derive(Debug, Clone)]
pub struct LogQuery {
    pub from: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: Option<usize>,
    pub start: Option<usize>,
    pub order: Order,
    pub levels: Vec<String>,
    pub fields: Vec<String>,
    pub search_term: Option<Regex>,
    pub filter: Option<QueryNode>,
}

#[derive(Debug, Clone)]
pub enum Order {
    Ascending,
    Descending,
}

impl FromStr for Order {
    type Err = String;

    fn from_str(input: &str) -> Result<Order, Self::Err> {
        match input.to_lowercase().as_str() {
            "asc" | "ascending" => Ok(Order::Ascending),
            "desc" | "descending" => Ok(Order::Descending),
            _ => Err(format!("Invalid order: {}", input)),
        }
    }
}

impl From<&str> for Order {
    fn from(s: &str) -> Self {
        Order::from_str(s).unwrap_or(Order::Descending)
    }
}

impl From<String> for Order {
    fn from(s: String) -> Self {
        Order::from_str(&s).unwrap_or(Order::Descending)
    }
}

impl From<i8> for Order {
    fn from(n: i8) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}
impl From<i16> for Order {
    fn from(n: i16) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}

impl From<i32> for Order {
    fn from(n: i32) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}

impl From<i64> for Order {
    fn from(n: i64) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}

impl From<i128> for Order {
    fn from(n: i128) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}

impl From<isize> for Order {
    fn from(n: isize) -> Self {
        if n == 1 {
            Order::Ascending
        } else {
            Order::Descending
        }
    }
}

// Helper trait to allow conversion from various types to Option<DateTime<Utc>>
pub trait IntoDateTimeOption {
    fn into_datetime_option(self) -> Option<DateTime<Utc>>;
}

impl IntoDateTimeOption for DateTime<Utc> {
    fn into_datetime_option(self) -> Option<DateTime<Utc>> {
        Some(self)
    }
}

impl IntoDateTimeOption for &str {
    fn into_datetime_option(self) -> Option<DateTime<Utc>> {
        LogQuery::parse_time(self)
    }
}

impl IntoDateTimeOption for String {
    fn into_datetime_option(self) -> Option<DateTime<Utc>> {
        LogQuery::parse_time(&self)
    }
}

impl LogQuery {
    pub fn new() -> Self {
        LogQuery {
            from: Some(Utc::now() - Duration::days(1)),
            until: Some(Utc::now()),
            limit: Some(50),
            start: Some(0),
            order: Order::Descending,
            fields: Vec::new(),
            levels: Vec::new(),
            search_term: None,
            filter: None,
        }
    }

    fn parse_time(time_str: &str) -> Option<DateTime<Utc>> {
        parse_datetime(time_str)
            .ok()
            .map(|parsed_date| parsed_date.with_timezone(&Utc))
    }

    pub fn from<T: IntoDateTimeOption>(mut self, from: T) -> Self {
        self.from = from.into_datetime_option();
        self
    }

    pub fn until<T: IntoDateTimeOption>(mut self, until: T) -> Self {
        self.until = until.into_datetime_option();
        self
    }

    /*pub fn from_datetime<T: Into<DateTime<Utc>>>(mut self, from_time: T) -> Self {
        self.from = Some(from_time.into());
        self
    }

    pub fn until_datetime<T: Into<DateTime<Utc>>>(mut self, until_time: T) -> Self {
        self.until = Some(until_time.into());
        self
    }*/

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn start(mut self, start: usize) -> Self {
        self.start = Some(start);
        self
    }

    pub fn order<S: Into<Order>>(mut self, order: S) -> Self {
        self.order = order.into();
        self
    }

    pub fn levels<S: Into<String>>(mut self, levels: Vec<S>) -> Self {
        self.levels = levels.into_iter().map(Into::into).collect();
        self
    }

    pub fn fields<S: Into<String>>(mut self, fields: Vec<S>) -> Self {
        self.fields = fields.into_iter().map(Into::into).collect();
        self
    }

    pub fn search_term<S: AsRef<str>>(mut self, search_term: S) -> Self {
        self.search_term = Some(Regex::new(search_term.as_ref()).unwrap());
        self
    }

    pub fn filter<T: Into<QueryNode>>(mut self, filter: T) -> Self {
        self.filter = Some(filter.into());
        self
    }
}

impl Default for LogQuery {
    fn default() -> Self {
        LogQuery::new()
    }
}

#[cfg(test)]
mod test {
    use chrono::{NaiveDate, TimeZone};

    use super::*;

    #[test]
    fn test_log_query_from_and_until_with_string() {
        let query = LogQuery::new()
            .from("2024-01-01T00:00:00Z")
            .until("2024-01-02T00:00:00Z");

        assert!(query.from.is_some());
        assert!(query.until.is_some());
        assert_eq!(
            query.from.unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(
            query.until.unwrap(),
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap()
        );
    }

    #[test]
    fn test_log_query_from_and_until_with_datetime() {
        let from_dt = Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap();
        let until_dt = Utc.with_ymd_and_hms(2023, 1, 2, 0, 0, 0).unwrap();

        let query = LogQuery::new().from(from_dt).until(until_dt);

        assert!(query.from.is_some());
        assert!(query.until.is_some());
        assert_eq!(query.from.unwrap(), from_dt);
        assert_eq!(query.until.unwrap(), until_dt);
    }

    // NEW TEST: Validate that `from` and `until` can handle types that are `Into<DateTime<Utc>>`
    // by explicitly calling `.into()` on them first. This invalidates the need for `from_datetime`.
    #[test]
    fn test_from_and_until_handle_into_datetime_via_explicit_into() {
        // A simple custom struct that implements `Into<DateTime<Utc>>`
        struct MyCustomTimeInput {
            year: i32,
            month: u32,
            day: u32,
            hour: u32,
            minute: u32,
            second: u32,
        }

        impl Into<DateTime<Utc>> for MyCustomTimeInput {
            fn into(self) -> DateTime<Utc> {
                let naive = NaiveDate::from_ymd_opt(self.year, self.month, self.day)
                    .unwrap()
                    .and_hms_opt(self.hour, self.minute, self.second)
                    .unwrap();
                Utc.from_utc_datetime(&naive)
            }
        }

        // Create an instance of our custom type
        let custom_from = MyCustomTimeInput {
            year: 2021,
            month: 1,
            day: 15,
            hour: 8,
            minute: 30,
            second: 0,
        };

        let custom_until = MyCustomTimeInput {
            year: 2021,
            month: 1,
            day: 16,
            hour: 9,
            minute: 0,
            second: 0,
        };

        // Convert custom type to DateTime<Utc> using .into() BEFORE passing to `from`/`until`
        let from_utc: DateTime<Utc> = custom_from.into();
        let until_utc: DateTime<Utc> = custom_until.into();

        let query = LogQuery::new()
            .from(from_utc) // This works because `from_utc` is DateTime<Utc>, which implements IntoDateTimeOption
            .until(until_utc); // Same here

        assert!(query.from.is_some());
        assert!(query.until.is_some());
        assert_eq!(
            query.from.unwrap(),
            Utc.with_ymd_and_hms(2021, 1, 15, 8, 30, 0).unwrap()
        );
        assert_eq!(
            query.until.unwrap(),
            Utc.with_ymd_and_hms(2021, 1, 16, 9, 0, 0).unwrap()
        );

        // You could also do it inline:
        let query2 = LogQuery::new()
            .from(Into::<DateTime<Utc>>::into(MyCustomTimeInput {
                year: 2020,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            }))
            .until(Into::<DateTime<Utc>>::into(MyCustomTimeInput {
                year: 2020,
                month: 1,
                day: 2,
                hour: 0,
                minute: 0,
                second: 0,
            }));

        assert!(query2.from.is_some());
        assert!(query2.until.is_some());
        assert_eq!(
            query2.from.unwrap(),
            Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(
            query2.until.unwrap(),
            Utc.with_ymd_and_hms(2020, 1, 2, 0, 0, 0).unwrap()
        );
    }
}
