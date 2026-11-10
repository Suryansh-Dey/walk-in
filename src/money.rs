//! Money is stored as integer minor units so totals never pick up float errors.

/// Currencies PayPal accepts on Orders.
pub const PAYPAL_CURRENCIES: &[&str] = &[
    "AUD", "BRL", "CAD", "CHF", "CNY", "CZK", "DKK", "EUR", "GBP", "HKD", "HUF", "ILS", "JPY", "MXN",
    "MYR", "NOK", "NZD", "PHP", "PLN", "SEK", "SGD", "THB", "TWD", "USD",
];

pub fn is_supported_currency(code: &str) -> bool {
    PAYPAL_CURRENCIES.contains(&code)
}

fn decimals(currency: &str) -> u32 {
    if matches!(currency, "HUF" | "JPY" | "TWD") { 0 } else { 2 }
}

pub fn to_minor(amount: f64, currency: &str) -> i64 {
    (amount * 10f64.powi(decimals(currency) as i32)).round() as i64
}

/// Formats minor units as the decimal string PayPal expects, such as "42.50".
pub fn format_minor(minor: i64, currency: &str) -> String {
    let places = decimals(currency);
    if places == 0 {
        return minor.to_string();
    }
    let scale = 10i64.pow(places);
    format!("{}.{:02}", minor / scale, (minor % scale).abs())
}

pub fn display_money(minor: i64, currency: &str) -> String {
    format!("{} {currency}", format_minor(minor, currency))
}

/// Rescales stored minor units when a shop moves between currencies with different decimals.
pub fn convert_minor(minor: i64, from: &str, to: &str) -> i64 {
    let (from, to) = (decimals(from), decimals(to));
    if to >= from { minor * 10i64.pow(to - from) } else { minor / 10i64.pow(from - to) }
}

/// Reads the number out of price text such as "$1,299.50", "12,50 €" or "¥4800".
pub fn parse_price_text(text: &str) -> Option<f64> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let raw: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | ' '))
        .filter(|c| *c != ' ')
        .collect();
    let digits = raw.trim_end_matches(['.', ',']);
    let separator = digits.rfind(['.', ',']);
    // A separator followed by exactly three digits is a thousands mark, not a decimal.
    let normalized = match separator {
        Some(at) if digits.len() - at - 1 != 3 => {
            format!("{}.{}", digits[..at].replace(['.', ','], ""), &digits[at + 1..])
        }
        _ => digits.replace(['.', ','], ""),
    };
    normalized.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_price_formats() {
        assert_eq!(parse_price_text("$42.00"), Some(42.0));
        assert_eq!(parse_price_text("$1,299.50"), Some(1299.5));
        assert_eq!(parse_price_text("12,50 €"), Some(12.5));
        assert_eq!(parse_price_text("1.299,50 EUR"), Some(1299.5));
        assert_eq!(parse_price_text("¥4,800"), Some(4800.0));
        assert_eq!(parse_price_text("from $9"), Some(9.0));
        assert_eq!(parse_price_text("free"), None);
    }

    #[test]
    fn formats_minor_units() {
        assert_eq!(format_minor(4250, "USD"), "42.50");
        assert_eq!(format_minor(5, "USD"), "0.05");
        assert_eq!(format_minor(4800, "JPY"), "4800");
        assert_eq!(to_minor(19.99, "USD"), 1999);
        assert_eq!(to_minor(4800.0, "JPY"), 4800);
    }

    #[test]
    fn converts_between_decimal_places() {
        assert_eq!(convert_minor(4800, "JPY", "USD"), 480_000);
        assert_eq!(convert_minor(480_000, "USD", "JPY"), 4800);
        assert_eq!(convert_minor(1234, "USD", "EUR"), 1234);
    }
}
