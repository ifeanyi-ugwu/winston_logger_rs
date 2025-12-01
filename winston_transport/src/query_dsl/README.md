# query_dsl

A Rust library for building type-safe, composable queries against structured data (like `serde_json::Value`).

This module provides a Domain Specific Language (DSL) that allows you to define complex filtering logic using a programmatic API.

## Usage

```rust
use query_dsl::prelude::*;
use serde_json::json;

fn main() {
    let query = and!(
        field_query!("user.age", gt(18)),
        or!(
            field_query!("user.status", eq("active")),
            field_query!("user.role", eq("admin"))
        )
    );

    let data1 = json!({ "user": { "age": 25, "status": "active" } });
    let data2 = json!({ "user": { "age": 30, "role": "admin" } });
    let data3 = json!({ "user": { "age": 15, "status": "inactive" } });

    println!("Data 1 matches: {}", query.evaluate(&data1)); // Output: true
    println!("Data 2 matches: {}", query.evaluate(&data2)); // Output: true
    println!("Data 3 matches: {}", query.evaluate(&data3)); // Output: false
}
```

## Constructing Queries from JSON

The query structure demonstrated above can also be constructed from JSON.

```json
{
  "$and": [
    { "user.age": { "$gt": 18 } },
    {
      "$or": [
        { "user.status": { "$eq": "active" } },
        { "user.role": { "$eq": "admin" } }
      ]
    }
  ]
}
```

```rust
use serde_json::json;

fn main() {
    let query = json!({
        "$and": [
            { "user.age": { "$gt": 18 } },
            {
                "$or": [
                    { "user.status": { "$eq": "active" } },
                    { "user.role": { "$eq": "admin" } }
                ]
            }
        ]
    })

    let data1 = json!({ "user": { "age": 25, "status": "active" } });
    let data2 = json!({ "user": { "age": 30, "role": "admin" } });
    let data3 = json!({ "user": { "age": 15, "status": "inactive" } });

    println!("Data 1 matches: {}", query.evaluate(&data1)); // Output: true
    println!("Data 2 matches: {}", query.evaluate(&data2)); // Output: true
    println!("Data 3 matches: {}", query.evaluate(&data3)); // Output: false
}
```

This functionality is inspired by the structure of MongoDB queries, allowing you to define your query logic in a familiar JSON format.
