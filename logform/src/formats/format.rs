pub trait Format {
    type Input;

    fn transform(&self, input: Self::Input) -> Option<Self::Input>;

    fn chain<F>(self, next: F) -> ChainedFormat<Self, F>
    where
        Self: Sized,
        F: Format<Input = Self::Input>,
    {
        ChainedFormat { first: self, next }
    }
}

pub struct ChainedFormat<F1, F2> {
    first: F1,
    next: F2,
}

impl<T, F1, F2> Format for ChainedFormat<F1, F2>
where
    F1: Format<Input = T>,
    F2: Format<Input = T>,
{
    type Input = T;

    fn transform(&self, input: T) -> Option<T> {
        self.first
            .transform(input)
            .and_then(|res| self.next.transform(res))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Example format implementations
    struct UpperCase;
    impl Format for UpperCase {
        type Input = String;

        fn transform(&self, input: String) -> Option<Self::Input> {
            Some(input.to_uppercase())
        }
    }

    struct ReverseFormat;
    impl Format for ReverseFormat {
        type Input = String;

        fn transform(&self, input: String) -> Option<Self::Input> {
            Some(input.chars().rev().collect())
        }
    }

    #[derive(Clone)]
    struct AddSuffix(String);
    impl Format for AddSuffix {
        type Input = String;

        fn transform(&self, input: String) -> Option<Self::Input> {
            Some(format!("{}{}", input, self.0))
        }
    }

    #[test]
    fn test_format() {
        let upper = UpperCase;
        let reverse = ReverseFormat;
        let suffix = AddSuffix("-end".to_string());

        let format = upper.chain(reverse).chain(suffix);

        let result = format.transform("hello".to_string());

        assert_eq!(result, Some("OLLEH-end".to_string()));
    }
}
