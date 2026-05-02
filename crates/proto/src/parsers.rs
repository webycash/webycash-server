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

    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Any digits-only or `digits.digits` string is recognised in
        /// full by amount_parser.
        #[test]
        fn amount_parser_accepts_canonical_decimal(
            whole in "[0-9]{1,18}",
            frac in proptest::option::of("[0-9]{1,8}"),
        ) {
            let s = match &frac {
                Some(f) => format!("{whole}.{f}"),
                None => whole.clone(),
            };
            let (rest, matched) = amount_parser(&s).expect("parse");
            prop_assert_eq!(rest, "");
            prop_assert_eq!(matched, s.as_str());
        }

        /// Trailing non-digit chars are left for the next parser
        /// (the production wire format uses `:` as the next separator).
        #[test]
        fn amount_parser_stops_at_first_non_digit(
            whole in "[0-9]{1,8}",
            sep in ":|;|,| ",
            tail in "[a-zA-Z0-9]{0,32}",
        ) {
            let s = format!("{whole}{sep}{tail}");
            let (rest, matched) = amount_parser(&s).expect("parse");
            prop_assert_eq!(matched, whole.as_str());
            // The separator + tail must remain unconsumed.
            prop_assert!(rest.starts_with(sep.as_str()));
        }

        /// amount_parser rejects any input that doesn't START with a digit.
        #[test]
        fn amount_parser_rejects_non_digit_prefix(s in "[^0-9].*") {
            prop_assert!(amount_parser(&s).is_err());
        }

        /// hex64 returns the first 64 chars when the prefix is hex.
        #[test]
        fn hex64_consumes_exactly_64_chars(prefix in "[0-9a-fA-F]{64}", suffix: String) {
            let s = format!("{prefix}{suffix}");
            let (rest, matched) = hex64(&s).expect("parse");
            prop_assert_eq!(matched.len(), 64);
            prop_assert_eq!(matched, prefix.as_str());
            prop_assert_eq!(rest, suffix.as_str());
        }

        /// hex64 rejects any input shorter than 64 hex chars.
        #[test]
        fn hex64_rejects_short(prefix in "[0-9a-fA-F]{0,63}") {
            prop_assert!(hex64(&prefix).is_err());
        }

        /// hex64 rejects when ANY of the first 64 chars is non-hex.
        #[test]
        fn hex64_rejects_non_hex_within_first_64(
            prefix in "[0-9a-fA-F]{0,63}",
            // A non-hex ASCII byte
            bad in "[g-zG-Z!@#$%^&*]",
        ) {
            let needed = 64 - prefix.len();
            // Pad with the bad byte and then more hex up to >64 total.
            let s = format!("{prefix}{bad}{}", "0".repeat(needed.max(1) + 8));
            prop_assert!(hex64(&s).is_err(), "should reject {s:?}");
        }
    }
}
