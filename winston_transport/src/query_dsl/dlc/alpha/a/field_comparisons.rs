use super::{comparator::Comparator, query_value::QueryValue};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct FieldComparison {
    pub comparator: Comparator,
    pub value: QueryValue,
}

impl FieldComparison {
    pub fn evaluate(&self, field_value: &Value) -> bool {
        self.comparator
            .compare(field_value, &Some(self.value.clone()))
    }

    pub fn gt(value: impl Into<QueryValue>) -> Self {
        Self {
            comparator: Comparator::GreaterThan,
            value: value.into(),
        }
    }

    pub fn lt(value: impl Into<QueryValue>) -> Self {
        Self {
            comparator: Comparator::LessThan,
            value: value.into(),
        }
    }

    pub fn eq(value: impl Into<QueryValue>) -> Self {
        Self {
            comparator: Comparator::Equals,
            value: value.into(),
        }
    }
}

// Macro to generate re-export functions in the prelude
macro_rules! export_field_comparison_constructors {
    ($($name:ident($($arg_name:ident: $arg_type:ty),*) $(,)?)* ) => {
        $(
            pub fn $name($($arg_name: $arg_type),*) -> FieldComparison {
                FieldComparison::$name($($arg_name),*)
            }
        )*
    }
}

/// This prelude just re-exports the shorthand constructors from FieldComparison
pub mod prelude {
    use super::*;

    // Re-export struct itself
    use crate::query_dsl::dlc::alpha::a::FieldComparison;

    /* pub fn gt(value: impl Into<QueryValue>) -> FieldComparison {
        FieldComparison::gt(value)
    }

    pub fn lt(value: impl Into<QueryValue>) -> FieldComparison {
        FieldComparison::lt(value)
    }

    pub fn eq(value: impl Into<QueryValue>) -> FieldComparison {
        FieldComparison::eq(value)
    }*/

    export_field_comparison_constructors! {
        gt(value: impl Into<QueryValue>),
        lt(value: impl Into<QueryValue>),
        eq(value: impl Into<QueryValue>),
    }
}
