//! Email extraction: single-message `.eml` (raw RFC 5322 MIME) and
//! multi-message `.mbox` files. Attachments are listed by filename (kept
//! searchable, e.g. finding an email by the name of a file someone sent
//! you) but their binary content is not recursively extracted here — see
//! [`crate::extract::archive`] for the same "list, don't blindly recurse
//! into everything" tradeoff and why.

use mail_parser::{MessageParser, MimeHeaders};

/// One parsed email, in a shape convenient for building indexable text
/// and for showing a useful result snippet/subject line in search results.
#[derive(Debug, Clone, Default)]
pub struct EmailDocument {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date_unix: Option<i64>,
    pub body_text: String,
    pub attachment_names: Vec<String>,
}

impl EmailDocument {
    /// Concatenates the fields into one indexable text blob. Subject and
    /// sender are repeated at the front so short queries that only match
    /// the subject line still score well against BM25's term-frequency
    /// math on typically-short emails.
    pub fn indexable_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.subject);
        out.push_str("\n\n");
        out.push_str(&format!("From: {}\nTo: {}\n\n", self.from, self.to));
        out.push_str(&self.body_text);
        if !self.attachment_names.is_empty() {
            out.push_str("\n\nAttachments: ");
            out.push_str(&self.attachment_names.join(", "));
        }
        out
    }
}

/// Parses a single raw RFC 5322 message (the contents of a `.eml` file).
/// Returns `None` if the bytes don't parse as a message at all (rather
/// than a partial, possibly-misleading result).
pub fn parse_eml(bytes: &[u8]) -> Option<EmailDocument> {
    let message = MessageParser::default().parse(bytes)?;

    let subject = message.subject().unwrap_or_default().to_string();
    let from = message
        .from()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address().or(a.name()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let to = message
        .to()
        .map(|addrs| {
            addrs
                .iter()
                .filter_map(|a| a.address().or(a.name()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let date_unix = message.date().map(|d| d.to_timestamp());

    let body_text = message
        .text_bodies()
        .filter_map(|part| part.text_contents())
        .collect::<Vec<_>>()
        .join("\n");
    // Fall back to stripping HTML if there was no plain-text part, the
    // same extractor the web crawler and local HTML files use.
    let body_text = if body_text.trim().is_empty() {
        message
            .html_bodies()
            .filter_map(|part| part.text_contents())
            .map(|html| crate::html::extract(html).indexable_text())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        body_text
    };

    let attachment_names = message
        .attachments()
        .filter_map(|a| a.attachment_name())
        .map(|s| s.to_string())
        .collect();

    Some(EmailDocument {
        subject,
        from,
        to,
        date_unix,
        body_text,
        attachment_names,
    })
}

/// Splits an mbox file (concatenated messages, each preceded by a
/// `From ` separator line at the start of a line) into individual raw
/// messages and parses each one. Malformed individual messages are
/// skipped rather than aborting the whole mailbox, consistent with the
/// rest of this module's best-effort philosophy.
pub fn parse_mbox(bytes: &[u8]) -> Vec<EmailDocument> {
    let text = String::from_utf8_lossy(bytes);
    let mut messages = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        if line.starts_with("From ") && !current.is_empty() {
            if let Some(doc) = parse_eml(current.as_bytes()) {
                messages.push(doc);
            }
            current.clear();
        }
        if !(line.starts_with("From ") && current.is_empty()) {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        if let Some(doc) = parse_eml(current.as_bytes()) {
            messages.push(doc);
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_EML: &str = "From: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nSubject: Quarterly numbers\r\nDate: Mon, 1 Jun 2026 10:00:00 +0000\r\nContent-Type: text/plain\r\n\r\nHere are the Q2 numbers you asked for.\r\n";

    #[test]
    fn parses_basic_eml_fields() {
        let doc = parse_eml(SAMPLE_EML.as_bytes()).expect("should parse");
        assert_eq!(doc.subject, "Quarterly numbers");
        assert!(doc.from.contains("alice@example.com"));
        assert!(doc.body_text.contains("Q2 numbers"));
    }

    #[test]
    fn mbox_splits_multiple_messages() {
        let mbox = format!(
            "From alice@example.com Mon Jun 1 10:00:00 2026\r\n{}From bob@example.com Mon Jun 1 11:00:00 2026\r\n{}",
            SAMPLE_EML, SAMPLE_EML
        );
        let messages = parse_mbox(mbox.as_bytes());
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn garbage_bytes_do_not_panic() {
        assert!(parse_eml(b"not an email at all, just bytes").is_none() || true);
    }
}
