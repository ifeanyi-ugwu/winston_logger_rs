use super::{
    field_comparisons::FieldComparison, field_path::FieldPath, query_value::QueryValue, FieldLogic,
    FieldNode, FieldQueryNode, LogicalOperator, QueryLogicNode, QueryNode,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum JsonQueryNode {
    LogicOp(HashMap<String, Vec<JsonQueryNode>>),
    FieldQuery(HashMap<String, HashMap<String, Value>>),
}

impl From<JsonQueryNode> for QueryNode {
    fn from(raw: JsonQueryNode) -> Self {
        match raw {
            JsonQueryNode::LogicOp(logic_map) => {
                assert_eq!(logic_map.len(), 1, "Only one logical operator per object");
                let (op_str, sub_queries) = logic_map.into_iter().next().unwrap();
                let operator = match op_str.as_str() {
                    "$and" => LogicalOperator::And,
                    "$or" => LogicalOperator::Or,
                    _ => panic!("Unknown logical operator: {}", op_str),
                };
                let children = sub_queries.into_iter().map(Into::into).collect();
                QueryLogicNode { operator, children }.into()
            }
            JsonQueryNode::FieldQuery(mut field_map) => {
                assert_eq!(field_map.len(), 1, "Only one field per condition");
                let (field_path, raw_field_query) = field_map.drain().next().unwrap();
                let path = FieldPath::from(field_path);
                let field_node = FieldNode::from(raw_field_query);
                FieldQueryNode::new(path, field_node).into()
            }
        }
    }
}

// Helper extension for FieldLogic to add multiple nodes
impl FieldLogic {
    pub fn with_nodes<T: Into<FieldNode>>(mut self, nodes: Vec<T>) -> Self {
        for node in nodes {
            self.conditions.push(node.into());
        }
        self
    }
}

impl From<HashMap<String, Value>> for FieldNode {
    fn from(op_map: HashMap<String, Value>) -> Self {
        assert_eq!(op_map.len(), 1, "Expected single operator in field query");
        let (op_str, value) = op_map.into_iter().next().unwrap();
        match op_str.as_str() {
            "$and" | "$or" => {
                if let Value::Array(sub_conditions) = value {
                    let operator = match op_str.as_str() {
                        "$and" => LogicalOperator::And,
                        "$or" => LogicalOperator::Or,
                        _ => unreachable!(),
                    };
                    let children: Vec<FieldNode> = sub_conditions
                        .into_iter()
                        .map(|sub_cond| {
                            if let Value::Object(map) = sub_cond {
                                HashMap::from_iter(map.into_iter().map(|(k, v)| (k, v))).into()
                            } else {
                                panic!("Expected object in logical sub-condition array");
                            }
                        })
                        .collect();
                    FieldNode::Logic(FieldLogic::new(operator).with_nodes(children))
                } else {
                    panic!("Expected array for logical operator value");
                }
            }
            _ => {
                let val = QueryValue::from(value);
                let comp = match op_str.as_str() {
                    "$eq" => FieldComparison::eq(val),
                    "$gt" => FieldComparison::gt(val),
                    "$lt" => FieldComparison::lt(val),
                    //"$ne" => FieldComparison::ne(val),
                    //"$in" => FieldComparison::in_array(val),
                    //"$nin" => FieldComparison::nin_array(val),
                    _ => panic!("Unknown field operator: {}", op_str),
                };
                FieldNode::Comparison(comp)
            }
        }
    }
}

impl From<Value> for QueryNode {
    fn from(value: Value) -> Self {
        let raw: JsonQueryNode =
            serde_json::from_value(value).expect("Failed to parse RawQuery from JSON Value");
        raw.into()
    }
}

//TODO: a way to convert the filter into mongodb object document search for usability with the mongodb transport
// weigh between doing it in the mongodb transport crate and doing it here, or even gating it in a feature so users don't compile what they are not using

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_mongodb_style_dsl() {
        let raw = json!({
            "$and": [
                { "user.age": { "$gt": 25 } },
                { "user.status": { "$or": [
                    { "$eq": "active" },
                    { "$eq": "pending" }
                ] } }
            ]
        });

        let parsed: JsonQueryNode = serde_json::from_value(raw).expect("Failed to parse RawQuery");
        println!("Parsed RawQuery: {:?}", parsed);
        let query: QueryNode = parsed.into();
        println!("Parsed QueryNode: {:?}", query);

        let user1 = json!({ "user": { "age": 30, "status": "active" } });
        let user2 = json!({ "user": { "age": 40, "status": "pending" } });
        let user3 = json!({ "user": { "age": 22, "status": "active" } });
        let user4 = json!({ "user": { "age": 30, "status": "inactive" } });

        assert!(query.evaluate(&user1), "user1 should match");
        assert!(query.evaluate(&user2), "user2 should match");
        assert!(!query.evaluate(&user3), "user3 too young");
        assert!(!query.evaluate(&user4), "user4 status mismatch");
    }

    #[test]
    fn test_nested_field_logic() {
        let raw = json!({
            "user.age": {
                "$and": [
                    { "$gt": 18 },
                    { "$lt": 65 }
                ]
            }
        });
        let parsed: JsonQueryNode = serde_json::from_value(raw).expect("Failed to parse RawQuery");
        let query: QueryNode = parsed.into();
        let user1 = json!({ "user": { "age": 30 } });
        let user2 = json!({ "user": { "age": 15 } });
        assert!(query.evaluate(&user1));
        assert!(!query.evaluate(&user2));
    }

    #[test]
    fn test_or_field_logic() {
        let _raw = json!({
            "user.status": {
                "$or": [
                    { "$eq": "active" },
                    { "$eq": "pending" }
                ]
            },
        });
        let raw = json!({
            "$and": [
                {
                    "user.status": {
                        "$or": [
                            { "$eq": "active" },
                            { "$eq": "pending" }
                        ]
                    }
                },
                {
                    "user.age": { "$gt": 25 }
                }
            ]
        });
        let parsed: JsonQueryNode = serde_json::from_value(raw).expect("Failed to parse RawQuery");
        let query: QueryNode = parsed.into();
        let user1 = json!({ "user": { "status": "active","age":26 } });
        let user2 = json!({ "user": { "status": "pending","age":26 } });
        let user3 = json!({ "user": { "status": "inactive","age":26 } });
        assert!(query.evaluate(&user1));
        assert!(query.evaluate(&user2));
        assert!(!query.evaluate(&user3));
    }

    #[test]
    fn test_nested_logic_in_field() {
        let raw = json!({
            "user.age": {
                "$and": [
                    { "$gt": 18 },
                    { "$or": [
                        { "$lt": 30 },
                        { "$gt": 50 }
                    ] }
                ]
            }
        });
        let parsed: JsonQueryNode = serde_json::from_value(raw).expect("Failed to parse RawQuery");
        let query: QueryNode = parsed.into();

        let user1 = json!({ "user": { "age": 25 } }); // Matches: > 18 and < 30
        let user2 = json!({ "user": { "age": 55 } }); // Matches: > 18 and > 50
        let user3 = json!({ "user": { "age": 35 } }); // Fails: > 18 but not < 30 or > 50
        let user4 = json!({ "user": { "age": 15 } }); // Fails: not > 18

        assert!(query.evaluate(&user1));
        assert!(query.evaluate(&user2));
        assert!(!query.evaluate(&user3));
        assert!(!query.evaluate(&user4));
    }

    #[test]
    fn test_deeply_nested_field_logic() {
        let raw = json!({
            "user.value": {
                "$and": [
                    { "$gt": 10 },
                    { "$or": [
                        { "$lt": 20 },
                        { "$and": [
                            { "$gt": 30 },
                            { "$lt": 40 }
                        ] }
                    ] }
                ]
            }
        });
        let parsed: JsonQueryNode = serde_json::from_value(raw).expect("Failed to parse RawQuery");
        let query: QueryNode = parsed.into();

        let obj1 = json!({ "user": { "value": 15 } }); // > 10 and < 20
        let obj2 = json!({ "user": { "value": 35 } }); // > 10 and (> 30 and < 40)
        let obj3 = json!({ "user": { "value": 25 } }); // > 10 but not < 20 and not (> 30 and < 40)
        let obj4 = json!({ "user": { "value": 5 } }); // not > 10

        assert!(query.evaluate(&obj1));
        assert!(query.evaluate(&obj2));
        assert!(!query.evaluate(&obj3));
        assert!(!query.evaluate(&obj4));
    }
}
