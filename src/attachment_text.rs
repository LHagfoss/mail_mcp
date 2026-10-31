//! Bounded text extraction for common mail attachment formats.

use std::io::{Cursor, Read};

use anyhow::{bail, Context};
use quick_xml::events::Event;

const MAX_ZIP_ENTRIES: usize = 512;

/// Extract readable text from a supported attachment, capped by Unicode chars.
pub fn extract(
    filename: &str,
    mime: &str,
    bytes: &[u8],
    max_chars: usize,
) -> anyhow::Result<String> {
    if max_chars == 0 {
        return Ok(String::new());
    }
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mime = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();

    let text = match mime.as_str() {
        "application/pdf" => {
            bail!("PDF extraction is disabled because PDF decompression is not safely bounded")
        }
        "text/html" => markup_text(bytes),
        "text/plain"
        | "text/csv"
        | "text/tab-separated-values"
        | "application/json"
        | "application/xml"
        | "text/xml" => String::from_utf8_lossy(bytes).into_owned(),
        _ if extension == "pdf" => {
            bail!("PDF extraction is disabled because PDF decompression is not safely bounded")
        }
        _ if matches!(extension.as_str(), "docx" | "xlsx") => {
            extract_office_xml(bytes, &extension)?
        }
        _ => bail!("unsupported attachment type {mime:?} ({filename})"),
    };
    Ok(text.chars().take(max_chars).collect())
}

fn extract_office_xml(bytes: &[u8], extension: &str) -> anyhow::Result<String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .context("Office attachment is not a readable ZIP archive")?;
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!("Office attachment has too many ZIP entries (limit {MAX_ZIP_ENTRIES})");
    }

    let mut output = String::new();
    let mut decoded_bytes = 0usize;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_ascii_lowercase();
        let wanted = if extension == "docx" {
            name == "word/document.xml"
                || name.starts_with("word/header")
                || name.starts_with("word/footer")
        } else {
            name == "xl/sharedstrings.xml" || name.starts_with("xl/worksheets/sheet")
        };
        if !wanted {
            continue;
        }
        let size = usize::try_from(entry.size()).context("Office XML entry is too large")?;
        decoded_bytes = decoded_bytes
            .checked_add(size)
            .context("Office attachment expanded size overflow")?;
        const MAX_EXPANDED_XML_BYTES: usize = 64 * 1024 * 1024;
        let expansion_limit = bytes
            .len()
            .saturating_mul(20)
            .max(1024 * 1024)
            .min(MAX_EXPANDED_XML_BYTES);
        if decoded_bytes > expansion_limit {
            bail!("Office attachment expands beyond the safe extraction limit");
        }
        let capacity = size;
        let read_limit = u64::try_from(size)
            .context("Office XML entry size exceeds the supported range")?
            .checked_add(1)
            .context("Office XML entry is too large")?;
        let mut xml = Vec::with_capacity(capacity);
        entry.take(read_limit).read_to_end(&mut xml)?;
        if xml.len() != size {
            bail!("Office XML entry expanded beyond its declared size");
        }
        output.push_str(&xml_text(&xml)?);
        output.push('\n');
    }
    if output.trim().is_empty() {
        bail!("no readable text found in Office attachment");
    }
    Ok(output)
}

fn xml_text(xml: &[u8]) -> anyhow::Result<String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut output = String::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Text(text) => {
                let decoded = text.decode()?;
                output.push_str(&quick_xml::escape::unescape(&decoded)?);
                output.push(' ');
            }
            Event::CData(text) => {
                output.push_str(&text.decode()?);
                output.push(' ');
            }
            Event::GeneralRef(reference) => {
                let character = if let Some(character) = reference.resolve_char_ref()? {
                    character
                } else {
                    match reference.decode()?.as_ref() {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "quot" => '\"',
                        "apos" => '\'',
                        name => bail!("unsupported XML entity reference &{name};"),
                    }
                };
                output.push(character);
                output.push(' ');
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(output.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn markup_text(bytes: &[u8]) -> String {
    let input = String::from_utf8_lossy(bytes);
    let mut text = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    decode_entities(&text)
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_caps_plain_text_and_html() {
        assert_eq!(
            extract("note.txt", "text/plain", b"abcdef", 3).unwrap(),
            "abc"
        );
        assert_eq!(
            extract("note.html", "text/html", b"<p>Hello &amp; goodbye</p>", 20).unwrap(),
            " Hello & goodbye "
        );
    }

    #[test]
    fn rejects_unsupported_attachment_types() {
        let error = extract("archive.bin", "application/octet-stream", b"bytes", 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported attachment type"));
    }

    #[test]
    fn rejects_pdf_until_extraction_is_resource_bounded() {
        for (filename, mime) in [
            ("scan.pdf", "application/pdf"),
            ("scan.pdf", "application/octet-stream"),
        ] {
            let error = extract(filename, mime, b"%PDF-1.7", 100).unwrap_err();
            assert!(error.to_string().contains("PDF extraction is disabled"));
        }
    }

    #[test]
    fn parses_xml_text_and_entities() {
        assert_eq!(
            xml_text(b"<root><t>Hi &amp; bye &#x21;</t></root>").unwrap(),
            "Hi & bye !"
        );
    }

    #[test]
    fn rejects_unknown_xml_entities() {
        assert!(xml_text(b"<root>&external;</root>").is_err());
    }
}
