/// A small demo module for exercising stream visuals.
/// This file exists only to generate interesting diffs.

pub fn greet(name: &str) -> String {
    format!("Hey there, {}! Welcome to the stream.", name)
}

pub fn farewell(name: &str) -> String {
    format!("Goodbye, {}. See you next time!", name)
}

pub fn fibonacci(n: u32) -> u64 {
    match n {
        0 => 0,
        1 => 1,
        _ => {
            let mut a: u64 = 0;
            let mut b: u64 = 1;
            for _ in 2..=n {
                let tmp = a + b;
                a = b;
                b = tmp;
            }
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_greet() {
        assert_eq!(greet("world"), "Hello, world!");
    }

    #[test]
    fn test_fibonacci() {
        assert_eq!(fibonacci(0), 0);
        assert_eq!(fibonacci(1), 1);
        assert_eq!(fibonacci(10), 55);
    }
}
