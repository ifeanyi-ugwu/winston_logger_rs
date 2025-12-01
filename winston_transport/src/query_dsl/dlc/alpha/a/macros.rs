#[macro_export]
macro_rules! and {
    ( $( $query:expr ),* ) => {
        $crate::query_dsl::dlc::alpha::a::QueryLogicNode::new($crate::query_dsl::dlc::alpha::a::LogicalOperator::And)
            $( .with_node($query) )*
    };
}

#[macro_export]
macro_rules! or {
    ( $( $query:expr ),* ) => {
        $crate::query_dsl::dlc::alpha::a::QueryLogicNode::new($crate::query_dsl::dlc::alpha::a::LogicalOperator::Or)
            $( .with_node($query) )*
    };
}

#[macro_export]
macro_rules! field_query {
    // Accept a path and any expression that evaluates to FieldComparison or FieldLogic
    ($path:expr, $logic:expr) => {
        $crate::query_dsl::dlc::alpha::a::FieldQueryNode::new($path, $logic)
    };
}

#[macro_export]
macro_rules! field_logic {
    // Case 1: General AND using prelude functions or nested logic
    (and, $( $node:expr ),+ $(,)?) => {{
        let mut logic = $crate::query_dsl::dlc::alpha::a::FieldLogic::new(
            $crate::query_dsl::dlc::alpha::a::LogicalOperator::And
        );
        $(
            logic = logic.with_node($node);
        )+
        logic
    }};
    // Case 2: General OR using prelude functions or nested logic
    (or, $( $node:expr ),+ $(,)?) => {{
        let mut logic = $crate::query_dsl::dlc::alpha::a::FieldLogic::new(
            $crate::query_dsl::dlc::alpha::a::LogicalOperator::Or
        );
        $(
            logic = logic.with_node($node);
        )+
        logic
    }};
}

#[cfg(test)]
mod tests {
    use crate::query_dsl::dlc::alpha::a::field_comparisons::prelude::*;
    use serde_json::json;

    #[test]
    fn test_macro_usage() {
        let age_check = field_query!("user.age", field_logic!(and, gt(18), lt(65)));

        let status_check = field_query!("user.status", eq("active"));
        let role_check = field_query!("user.role", eq("admin"));

        let query = and!(age_check, or!(status_check, role_check));

        let _complex_query = and!(
            field_query!("user.age", gt(18)),
            or!(
                field_query!("user.status", eq("active")),
                and!(
                    field_query!("user.role", eq("admin")),
                    field_query!("user.permission", eq("admin"))
                )
            )
        );

        let _complex_age = field_query!(
            "user.age",
            field_logic!(and, gt(18), field_logic!(or, lt(65), eq(99)))
        );

        let _query3 = field_query!(
            "user.status",
            field_logic!(or, eq("active"), eq("inactive"))
        );

        let _advanced_query = and!(
            field_query!(
                // Field: user.age
                "user.age",
                field_logic!(
                    // Logic: AND
                    and,
                    gt(18),                           // Condition 1
                    field_logic!(or, lt(30), gt(50))  // Condition 2 (nested OR)
                )
            ),
            or!(
                // Combine age check with status/role check
                field_query!("user.status", eq("active")), // Field: user.status
                and!(
                    // Nested AND for role/permissions
                    field_query!("user.role", eq("admin")), // Field: user.role
                    field_query!(
                        // Field: user.permissions
                        "user.permissions",
                        field_logic!(or, eq("read"), eq("write")) // Logic: OR
                    )
                )
            )
        );

        let json_data = json!({ "user": { "age": 30, "status": "inactive", "role": "admin" } });

        assert!(query.evaluate(&json_data));
    }
}
