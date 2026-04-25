//! Shared `nom` parser building blocks. Used by every asset crate.

use nom::bytes::complete::{tag, take_while_m_n};
use nom::character::complete::digit1;
use nom::combinator::{opt, recognize};
use nom::sequence::tuple;
use nom::IResult;

/// Recognise a decimal amount string: `digits[.digits]` (no leading `e`).
/// Returns the matched span; conversion to `Amount` is the caller's job.
pub fn amount_parser(input: &str) -> IResult<&str, &str> {
    recognize(tuple((digit1, opt(tuple((tag("."), digit1))))))(input)
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Recognise exactly 64 hex characters (for SHA256 hashes / 32-byte secrets).
pub fn hex64(input: &str) -> IResult<&str, &str> {
    take_while_m_n(64, 64, is_hex)(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_parser_basic() {
        assert_eq!(amount_parser("200").unwrap().1, "200");
        assert_eq!(amount_parser("1.50000000").unwrap().1, "1.50000000");
        assert_eq!(amount_parser("0.1:rest").unwrap().1, "0.1");
    }

    #[test]
    fn hex64_basic() {
        let s = "a".repeat(64);
        assert_eq!(hex64(&s).unwrap().1, &s);

        let too_short = "a".repeat(63);
        assert!(hex64(&too_short).is_err());

        let with_rest = format!("{}:after", "f".repeat(64));
        assert_eq!(hex64(&with_rest).unwrap().1, "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
    }
}
