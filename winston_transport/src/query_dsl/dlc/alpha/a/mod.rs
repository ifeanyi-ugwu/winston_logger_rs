pub mod comparator;
pub mod field_comparisons;
pub mod field_path;
mod json_query_node;
pub mod macros;
pub mod prelude;
mod query_value;

use field_comparisons::FieldComparison;
use field_path::FieldPath;
use query_value::QueryValue;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum LogicalOperator {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub enum QueryNode {
    FieldQuery(FieldQueryNode),
    Logic(QueryLogicNode),
}

impl QueryNode {
    pub fn evaluate(&self, value: &Value) -> bool {
        //println!("Evaluating QueryNode: {:?}", self);
        match self {
            QueryNode::FieldQuery(field_node) => field_node.evaluate(value),
            QueryNode::Logic(logical_node) => logical_node.evaluate(value),
        }
    }
}

#[derive(Debug, Clone)]
pub struct QueryLogicNode {
    operator: LogicalOperator,
    children: Vec<QueryNode>,
}

impl QueryLogicNode {
    pub fn new(operator: LogicalOperator) -> Self {
        QueryLogicNode {
            operator,
            children: Vec::new(),
        }
    }

    pub fn with_node<T: Into<QueryNode>>(mut self, node: T) -> Self {
        self.children.push(node.into());
        self
    }

    pub fn evaluate(&self, value: &Value) -> bool {
        match self.operator {
            LogicalOperator::And => self.children.iter().all(|child| child.evaluate(value)),
            LogicalOperator::Or => self.children.iter().any(|child| child.evaluate(value)),
        }
    }
}

impl From<QueryLogicNode> for QueryNode {
    fn from(node: QueryLogicNode) -> Self {
        QueryNode::Logic(node)
    }
}

impl From<FieldQueryNode> for QueryNode {
    fn from(node: FieldQueryNode) -> Self {
        QueryNode::FieldQuery(node)
    }
}

#[derive(Debug, Clone)]
pub enum FieldNode {
    Comparison(FieldComparison),
    Logic(FieldLogic),
}

impl FieldNode {
    pub fn evaluate(&self, field_value: &Value) -> bool {
        match self {
            FieldNode::Comparison(comp) => comp.evaluate(field_value),
            FieldNode::Logic(logic) => match logic.operator {
                LogicalOperator::And => logic.conditions.iter().all(|c| c.evaluate(&field_value)),
                LogicalOperator::Or => logic.conditions.iter().any(|c| c.evaluate(&field_value)),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldLogic {
    pub operator: LogicalOperator,
    pub conditions: Vec<FieldNode>,
}

impl FieldLogic {
    pub fn new(operator: LogicalOperator) -> Self {
        FieldLogic {
            operator,
            conditions: Vec::new(),
        }
    }

    pub fn with_node<T: Into<FieldNode>>(mut self, node: T) -> Self {
        self.conditions.push(node.into());
        self
    }
}

impl From<FieldComparison> for FieldNode {
    fn from(comp: FieldComparison) -> Self {
        FieldNode::Comparison(comp)
    }
}

impl From<FieldLogic> for FieldNode {
    fn from(logic: FieldLogic) -> Self {
        FieldNode::Logic(logic)
    }
}

#[derive(Debug, Clone)]
pub struct FieldQueryNode {
    pub(crate) path: FieldPath,
    pub(crate) node: FieldNode,
}

impl FieldQueryNode {
    pub fn new(path: impl Into<FieldPath>, node: impl Into<FieldNode>) -> Self {
        FieldQueryNode {
            path: path.into(),
            node: node.into(),
        }
    }

    pub fn evaluate(&self, value: &Value) -> bool {
        let Some(field_value) = self.path.extract(value) else {
            return false;
        };
        self.node.evaluate(&field_value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_query_and_field_nodes_usage() {
        let age_node = FieldQueryNode::new("user.age", FieldComparison::gt(25));

        let status_logic = FieldQueryNode::new(
            "user.status",
            FieldLogic::new(LogicalOperator::Or)
                .with_node(FieldComparison::eq("active"))
                .with_node(FieldComparison::eq("pending")),
        );

        let full_query = QueryLogicNode::new(LogicalOperator::And)
            .with_node(age_node)
            .with_node(status_logic);

        // Match 1: age > 25, status is "active" => true
        let json1 = json!({ "user": { "age": 30, "status": "active" } });
        assert!(full_query.evaluate(&json1));

        // Match 2: age > 25, status is "pending" => true
        let json2 = json!({ "user": { "age": 40, "status": "pending" } });
        assert!(full_query.evaluate(&json2));

        // Fail: age <= 25 => false
        let json3 = json!({ "user": { "age": 22, "status": "active" } });
        assert!(!full_query.evaluate(&json3));

        // Fail: age ok, but status is neither => false
        let json4 = json!({ "user": { "age": 35, "status": "inactive" } });
        assert!(!full_query.evaluate(&json4));
    }
}
