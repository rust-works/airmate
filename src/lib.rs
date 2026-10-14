//! airmate: a tool for routing AI requests.
//!
//! This is a placeholder. The binary currently prints a greeting and nothing else.

/// The line the `airmate` binary prints.
#[must_use]
pub fn greeting() -> &'static str {
    "Hello, world!"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_is_hello_world() {
        assert_eq!(greeting(), "Hello, world!");
    }
}
