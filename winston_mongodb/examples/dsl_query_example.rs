// Import the macros from winston_transport
use winston_transport::{and, field_logic, field_query, or};
// Import the comparison functions from the prelude
use winston_transport::query_dsl::dlc::alpha::a::prelude::*;
use winston_transport::LogQuery;

// This example demonstrates how to use the DSL query filter with MongoDB transport
//
// The DSL allows you to build complex queries programmatically:
// - Field comparisons: eq, gt, lt, gte, lte
// - Logical operators: and!, or! (macros)
// - Field logic: combine multiple conditions on the same field
//
// Example query structure:
// LogQuery::new()
//     .filter(and!(
//         field_query!("meta.user.age", gt(18)),
//         field_query!("meta.user.status", eq("active"))
//     ))

fn main() {
    // Example 1: Simple equality filter
    let query1 = LogQuery::new()
        .levels(vec!["info", "error"])
        .filter(field_query!("meta.user.id", eq(12345)));

    println!("Query 1 - Simple equality:");
    println!("{:#?}\n", query1);

    // Example 2: Age range filter (using field logic)
    let query2 = LogQuery::new().filter(field_query!(
        "meta.user.age",
        field_logic!(and, gt(18), lt(65))
    ));

    println!("Query 2 - Age range:");
    println!("{:#?}\n", query2);

    // Example 3: Complex multi-field filter
    let query3 = LogQuery::new()
        .from("2024-01-01T00:00:00Z")
        .until("2024-12-31T23:59:59Z")
        .levels(vec!["info", "warn"])
        .filter(and!(
            field_query!("meta.user.age", field_logic!(and, gt(18), lt(65))),
            field_query!("meta.user.status", eq("active"))
        ));

    println!("Query 3 - Complex multi-field:");
    println!("{:#?}\n", query3);

    // Example 4: Using OR logic
    let query4 = LogQuery::new().filter(or!(
        field_query!("meta.priority", eq("high")),
        field_query!("meta.priority", eq("critical"))
    ));

    println!("Query 4 - OR logic:");
    println!("{:#?}\n", query4);

    // Example 5: Nested logical operators
    let query5 = LogQuery::new().filter(and!(
        field_query!("meta.department", eq("engineering")),
        or!(
            field_query!("meta.role", eq("developer")),
            field_query!("meta.role", eq("architect"))
        )
    ));

    println!("Query 5 - Nested logic:");
    println!("{:#?}\n", query5);

    println!("\n=== MongoDB Filter Conversion ===");
    println!("To use these queries with MongoDB:");
    println!("1. Create a MongoDBTransport instance");
    println!("2. Call transport.query(&query) to execute the query");
    println!("3. The DSL filter will be automatically converted to MongoDB BSON filter");
    println!("\nExample MongoDB filter for Query 3:");
    println!("{{");
    println!("  \"$and\": [");
    println!("    {{ \"timestamp\": {{ \"$gte\": ISODate(...), \"$lte\": ISODate(...) }} }},");
    println!("    {{ \"level\": {{ \"$in\": [\"info\", \"warn\"] }} }},");
    println!("    {{");
    println!("      \"meta.user.age\": {{");
    println!("        \"$and\": [");
    println!("          {{ \"$gt\": 18 }},");
    println!("          {{ \"$lt\": 65 }}");
    println!("        ]");
    println!("      }}");
    println!("    }},");
    println!("    {{ \"meta.user.status\": {{ \"$eq\": \"active\" }} }}");
    println!("  ]");
    println!("}}");
}
