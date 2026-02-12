use mongodb::bson::{doc, Bson, Document};
use winston_transport::query_dsl::dlc::alpha::a::{
    comparator::Comparator, field_comparisons::FieldComparison, FieldLogic, FieldNode,
    FieldQueryNode, LogicalOperator, QueryLogicNode, QueryNode, QueryValue,
};

pub trait ToMongoDbFilter {
    fn to_mongodb_filter(&self) -> Document;
}

impl ToMongoDbFilter for QueryNode {
    fn to_mongodb_filter(&self) -> Document {
        match self {
            QueryNode::Logic(logic_node) => logic_node.to_mongodb_filter(),
            QueryNode::FieldQuery(field_query_node) => field_query_node.to_mongodb_filter(),
        }
    }
}

impl ToMongoDbFilter for QueryLogicNode {
    fn to_mongodb_filter(&self) -> Document {
        let op_str = match self.operator() {
            LogicalOperator::And => "$and",
            LogicalOperator::Or => "$or",
        };
        let mut sub_filters = Vec::new();
        for child in self.children() {
            sub_filters.push(child.to_mongodb_filter());
        }
        doc! { op_str: sub_filters }
    }
}

impl ToMongoDbFilter for FieldQueryNode {
    fn to_mongodb_filter(&self) -> Document {
        let field_path = field_path_to_string(self.path());

        match self.node() {
            FieldNode::Comparison(comp) => {
                doc! { field_path: comp.to_mongodb_filter() }
            }
            FieldNode::Logic(logic) => {
                match logic.operator {
                    LogicalOperator::And => {
                        // AND logic on same field merges operators into single document
                        // Example: age > 18 AND age < 65 becomes { "age": { "$gt": 18, "$lt": 65 } }
                        doc! { field_path: logic.to_mongodb_filter() }
                    }
                    LogicalOperator::Or => {
                        // OR logic on same field expands to multiple conditions at document level
                        // Example: status = "a" OR status = "b" becomes { "$or": [ { "status": { "$eq": "a" } }, { "status": { "$eq": "b" } } ] }
                        let mut or_conditions = Vec::new();
                        for condition in &logic.conditions {
                            or_conditions
                                .push(doc! { field_path.clone(): condition.to_mongodb_filter() });
                        }
                        doc! { "$or": or_conditions }
                    }
                }
            }
        }
    }
}

impl ToMongoDbFilter for FieldNode {
    fn to_mongodb_filter(&self) -> Document {
        match self {
            FieldNode::Logic(field_logic) => field_logic.to_mongodb_filter(),
            FieldNode::Comparison(field_comparison) => field_comparison.to_mongodb_filter(),
        }
    }
}

impl ToMongoDbFilter for FieldLogic {
    fn to_mongodb_filter(&self) -> Document {
        // AND logic on same field merges all operators into one document
        // Example: age > 18 AND age < 65 becomes { "$gt": 18, "$lt": 65 }
        // OR logic is handled in FieldQueryNode to avoid invalid MongoDB syntax
        let mut merged_doc = Document::new();
        for condition in &self.conditions {
            let condition_doc = condition.to_mongodb_filter();
            for (key, value) in condition_doc {
                merged_doc.insert(key, value);
            }
        }
        merged_doc
    }
}

impl ToMongoDbFilter for FieldComparison {
    fn to_mongodb_filter(&self) -> Document {
        match &self.comparator {
            Comparator::Equals => doc! { "$eq": value_to_bson(&self.value) },
            Comparator::NotEquals => doc! { "$ne": value_to_bson(&self.value) },
            Comparator::GreaterThan => doc! { "$gt": value_to_bson(&self.value) },
            Comparator::LessThan => doc! { "$lt": value_to_bson(&self.value) },
            Comparator::GreaterThanOrEqual => doc! { "$gte": value_to_bson(&self.value) },
            Comparator::LessThanOrEqual => doc! { "$lte": value_to_bson(&self.value) },
            Comparator::In => doc! { "$in": value_to_bson(&self.value) },
            Comparator::NotIn => doc! { "$nin": value_to_bson(&self.value) },
            Comparator::Exists => doc! { "$exists": true },
            Comparator::NotExists => doc! { "$exists": false },
            Comparator::Matches => {
                if let QueryValue::Regex(r) = &self.value {
                    doc! { "$regex": r.as_str() }
                } else {
                    doc! {}
                }
            }
            Comparator::NotMatches => {
                if let QueryValue::Regex(r) = &self.value {
                    doc! { "$not": { "$regex": r.as_str() } }
                } else {
                    doc! {}
                }
            }
            // Comparators without direct MongoDB mapping default to equality check
            // Complex comparators may require custom handling or are not supported
            _ => {
                doc! { "$eq": value_to_bson(&self.value) }
            }
        }
    }
}

// Helper function to convert FieldPath to a string representation
fn field_path_to_string(
    path: &winston_transport::query_dsl::dlc::alpha::a::field_path::FieldPath,
) -> String {
    use winston_transport::query_dsl::dlc::alpha::a::field_path::PathSegment;

    path.segments
        .iter()
        .map(|segment| match segment {
            PathSegment::Field(name) => name.clone(),
            PathSegment::Wildcard => "*".to_string(),
            PathSegment::ArrayIndex(idx) => format!("[{}]", idx),
            PathSegment::ArrayWildcard => "[*]".to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

// Helper function to convert QueryValue to bson::Bson
fn value_to_bson(query_value: &QueryValue) -> Bson {
    match query_value {
        QueryValue::String(s) => Bson::String(s.clone()),
        QueryValue::Number(n) => Bson::Double(*n),
        QueryValue::Boolean(b) => Bson::Boolean(*b),
        QueryValue::Null => Bson::Null,
        QueryValue::Array(arr) => Bson::Array(arr.iter().map(value_to_bson).collect()),
        QueryValue::Regex(r) => Bson::RegularExpression(mongodb::bson::Regex {
            pattern: r.as_str().to_string(),
            options: "".to_string(),
        }),
        QueryValue::DateTime(dt) => Bson::DateTime(mongodb::bson::DateTime::from_chrono(*dt)),
        QueryValue::Duration(dur) => Bson::Int64(dur.num_milliseconds()),
        QueryValue::Function(_) => {
            // Functions cannot be serialized to BSON, returning null
            // Function-based comparisons must be evaluated client-side
            Bson::Null
        }
    }
}
