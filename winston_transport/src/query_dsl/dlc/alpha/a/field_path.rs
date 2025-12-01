use serde_json::Value;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct FieldPath {
    pub segments: Vec<PathSegment>,
}

impl FieldPath {
    pub fn extract<'a>(&self, value: &'a Value) -> Option<Value> {
        let mut current_values = vec![value];

        for segment in &self.segments {
            let mut next_values = Vec::new();

            for current_value in &current_values {
                match (segment, current_value) {
                    // Extract a field from an object
                    (PathSegment::Field(field), Value::Object(map)) => {
                        if let Some(next_value) = map.get(field) {
                            next_values.push(next_value);
                        }
                    }
                    // Wildcard match: collect all values in an object
                    (PathSegment::Wildcard, Value::Object(map)) => {
                        next_values.extend(map.values());
                    }
                    // Extract an array index
                    (PathSegment::ArrayIndex(idx), Value::Array(arr)) => {
                        if let Some(next_value) = arr.get(*idx) {
                            next_values.push(next_value);
                        }
                    }
                    // Array wildcard: collect all elements in the array
                    (PathSegment::ArrayWildcard, Value::Array(arr)) => {
                        next_values.extend(arr);
                    }

                    _ => {}
                }
            }

            if next_values.is_empty() {
                return None;
            }

            current_values = next_values;
        }

        /*println!(
            "Extracting {:?} from {:?}, got {:?}",
            self, value, current_values
        );*/

        /*if current_values.len() == 1 {
            Some(current_values[0].clone())
        } else {
            // Otherwise, return a new `Value::Array`
            Some(Value::Array(current_values.into_iter().cloned().collect()))
        }*/
        match current_values.len() {
            0 => None,
            1 => Some(current_values[0].clone()),
            _ => Some(Value::Array(current_values.into_iter().cloned().collect())),
        }
    }

    pub fn extract_refs<'a>(&self, value: &'a Value) -> Vec<&'a Value> {
        let mut current_values = vec![value];

        for segment in &self.segments {
            let mut next_values = Vec::new();

            for current_value in &current_values {
                match (segment, current_value) {
                    (PathSegment::Field(field), Value::Object(map)) => {
                        if let Some(next_value) = map.get(field) {
                            next_values.push(next_value);
                        }
                    }
                    (PathSegment::Wildcard, Value::Object(map)) => {
                        next_values.extend(map.values());
                    }
                    (PathSegment::ArrayIndex(idx), Value::Array(arr)) => {
                        if let Some(next_value) = arr.get(*idx) {
                            next_values.push(next_value);
                        }
                    }
                    (PathSegment::ArrayWildcard, Value::Array(arr)) => {
                        next_values.extend(arr);
                    }
                    _ => {}
                }
            }

            if next_values.is_empty() {
                return vec![];
            }

            current_values = next_values;
        }

        current_values
    }
}

#[derive(Clone, Debug)]
pub enum PathSegment {
    Field(String),
    Wildcard,
    ArrayIndex(usize),
    ArrayWildcard,
}

impl FromStr for FieldPath {
    type Err = String;

    fn from_str(path: &str) -> Result<Self, Self::Err> {
        let segments = path
            .split('.')
            .flat_map(|segment| {
                if segment.contains('[') {
                    // Handle array-related segments
                    segment
                        .split('[')
                        .filter_map(|part| {
                            if part.is_empty() {
                                None
                            } else if part == "*]" {
                                Some(PathSegment::ArrayWildcard)
                            } else if part.ends_with(']') {
                                part[..part.len() - 1]
                                    .parse()
                                    .map(PathSegment::ArrayIndex)
                                    .ok()
                            } else {
                                Some(PathSegment::Field(part.to_string()))
                            }
                        })
                        .collect::<Vec<_>>()
                } else {
                    // Handle simple field segments
                    match segment {
                        "*" => vec![PathSegment::Wildcard],
                        s => vec![PathSegment::Field(s.to_string())],
                    }
                }
            })
            .collect();

        Ok(FieldPath { segments })
    }
}

impl From<&str> for FieldPath {
    fn from(s: &str) -> Self {
        s.parse().expect("Invalid field path")
    }
}

impl From<String> for FieldPath {
    fn from(s: String) -> Self {
        FieldPath::from(s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::Value;

    /// Helper function to compare JSON values in a test scenario. enabling `preserve_order` for serde_json just for this test is not rational
    fn assert_json_eq(actual: &Value, expected: &Value, context: &str) {
        match (actual, expected) {
            // Compare arrays: sort them first
            (Value::Array(actual_arr), Value::Array(expected_arr)) => {
                let mut actual_sorted = actual_arr.clone();
                let mut expected_sorted = expected_arr.clone();
                actual_sorted.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
                expected_sorted.sort_by(|a, b| a.to_string().cmp(&b.to_string()));

                assert_eq!(actual_sorted, expected_sorted, "{context}");
            }
            // Compare objects: order-preserving comparison
            (Value::Object(actual_obj), Value::Object(expected_obj)) => {
                assert_eq!(actual_obj, expected_obj, "{context}");
            }
            // Scalar values (String, Number, Bool, etc.)
            (actual, expected) => {
                assert_eq!(actual, expected, "{context}");
            }
        }
    }

    #[test]
    fn test_field_path_extraction() {
        let json_data = serde_json::json!({
            "user": {
                "name": "Alice",
                "address": { "city": "NY", "zipcode": "10001" },
                "age": 30
            },
            "items": [
                { "price": 10 },
                { "price": 20 }
            ]
        });

        let test_cases = vec![
            ("user.name", Some(Value::String("Alice".into()))),
            ("user.address.city", Some(Value::String("NY".into()))),
            ("items[1].price", Some(Value::Number(20.into()))),
            (
                "items[*].price",
                Some(Value::Array(vec![
                    Value::Number(10.into()),
                    Value::Number(20.into()),
                ])),
            ),
            ("user.address.street", None),
        ];

        for (path, expected) in test_cases {
            let field_path = FieldPath::from(path);
            let result = field_path.extract(&json_data);
            assert_eq!(result, expected, "Failed for path: {}", path);
        }

        let field_path = FieldPath::from("user.*");
        let result = field_path.extract(&json_data);

        assert_json_eq(
            &Some(Value::Array(vec![
                Value::String("Alice".into()),
                Value::Object(
                    serde_json::json!({ "city": "NY", "zipcode": "10001" })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                Value::Number(30.into()),
            ]))
            .unwrap(),
            &result.unwrap(),
            format!("Failed for path: {}", "user.*").as_str(),
        );
    }

    #[test]
    fn test_array_wildcard_with_wildcard() {
        let json_data = serde_json::json!({
            "users": [
                { "name": "Alice", "age": 30 },
                { "name": "Bob", "age": 25 }
            ]
        });

        let field_path = FieldPath::from("users[*].*");
        let result = field_path.extract(&json_data);

        assert_json_eq(
            &result.unwrap(),
            &serde_json::json!(["Alice", 30, "Bob", 25]),
            "Failed for path: users[*].*",
        );
    }
}
