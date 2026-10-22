use std::fmt::Write;

/// A random 128-bit secret, written as 32 lowercase hex digits.
#[derive(Clone)]
pub(crate) struct Token(String);

impl Token {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes)?;
        let mut hex = String::with_capacity(32);
        for byte in bytes {
            let _ = write!(hex, "{byte:02x}");
        }
        Ok(Self(hex))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compare without returning early on the first differing byte.
    pub fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.as_bytes();
        let candidate = candidate.as_bytes();
        if expected.len() != candidate.len() {
            return false;
        }
        expected
            .iter()
            .zip(candidate)
            .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }

    #[cfg(test)]
    pub fn from_hex(hex: &str) -> Self {
        Self(hex.to_string())
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::Token;

    #[test]
    fn tokens_are_32_hex_digits_and_differ() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_eq!(a.as_str().len(), 32);
        assert!(a.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn matches_is_exact() {
        let token = Token::from_hex("00112233445566778899aabbccddeeff");
        assert!(token.matches("00112233445566778899aabbccddeeff"));
        assert!(!token.matches("00112233445566778899aabbccddeefe"));
        assert!(!token.matches("00112233445566778899AABBCCDDEEFF"));
        assert!(!token.matches("00112233445566778899aabbccddeef"));
        assert!(!token.matches(""));
        assert!(!format!("{token:?}").contains("0011"));
    }
}
