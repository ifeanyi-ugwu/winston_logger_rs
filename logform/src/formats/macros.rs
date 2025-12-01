#[macro_export]
macro_rules! chain {
    ($first:expr $(, $rest:expr)+ $(,)?) => {{
        $first $(.chain($rest))*
    }};
}
