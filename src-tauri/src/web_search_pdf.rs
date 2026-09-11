//! Bounded, pure-Rust PDF support for standalone web search.
//!
//! PDF bytes never leave the process. Text extraction uses `pdf-extract` and
//! page rasterisation uses `hayro`, so an installed Vellum build does not
//! depend on PDFium, Poppler, Edge, Python, or a helper executable.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

const MAX_PDF_BYTES: usize = 20 * 1024 * 1024;
const MAX_RENDER_WIDTH: f32 = 1_600.0;
const MAX_RENDER_HEIGHT: f32 = 2_400.0;
const MAX_RENDER_SCALE: f32 = 2.0;
const MAX_PNG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct PdfPageCapture {
    pub png: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub page_count: usize,
    pub page_text: String,
}

fn validate_pdf(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 5 || &bytes[0..5] != b"%PDF-" {
        return Err("response is not a valid PDF".into());
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err(format!(
            "PDF exceeds the {} MiB processing limit",
            MAX_PDF_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// Extract all readable page text. A valid image-only PDF returns an explicit
/// note rather than binary bytes or a fabricated transcript.
pub fn extract_pdf_text(bytes: &[u8]) -> Result<String, String> {
    validate_pdf(bytes)?;
    let pages = pdf_extract::extract_text_from_mem_by_pages(bytes)
        .map_err(|error| format!("PDF text extraction failed: {error}"))?;
    let mut output = String::new();
    for (index, text) in pages.iter().enumerate() {
        let text = text.trim();
        output.push_str(&format!("--- PDF page {} ---\n", index + 1));
        if text.is_empty() {
            output.push_str("[no extractable text; the page may contain only images]\n");
        } else {
            output.push_str(text);
            output.push('\n');
        }
    }
    if pages.is_empty() {
        return Err("PDF contains no readable pages".into());
    }
    Ok(output)
}

/// Rasterise one zero-indexed PDF page to PNG and return its extracted text.
/// Rendering is CPU-only and dimensions are capped to bound memory use.
pub fn render_pdf_page(bytes: &[u8], pageno: usize) -> Result<PdfPageCapture, String> {
    validate_pdf(bytes)?;
    let data = Arc::new(bytes.to_vec());
    let pdf = hayro::Pdf::new(data)
        .map_err(|error| format!("PDF parser rejected the document: {error:?}"))?;
    let pages = pdf.pages();
    let page_count = pages.len();
    let page = pages.get(pageno).ok_or_else(|| {
        format!(
            "PDF page {} is out of range; document has {} pages",
            pageno, page_count
        )
    })?;

    let (source_width, source_height) = page.render_dimensions();
    if !source_width.is_finite()
        || !source_height.is_finite()
        || source_width <= 0.0
        || source_height <= 0.0
    {
        return Err("PDF page has invalid dimensions".into());
    }
    let initial_scale = MAX_RENDER_SCALE
        .min(MAX_RENDER_WIDTH / source_width)
        .min(MAX_RENDER_HEIGHT / source_height)
        .max(0.1);
    let mut scale = initial_scale;
    let (png, width, height) = loop {
        let width = (source_width * scale).ceil().clamp(1.0, u16::MAX as f32) as u16;
        let height = (source_height * scale).ceil().clamp(1.0, u16::MAX as f32) as u16;

        // PDF parsers/renderers process attacker-controlled data. Both crates
        // are safe Rust, but convert an unexpected panic into a request error.
        let png = catch_unwind(AssertUnwindSafe(|| {
            hayro::render(
                page,
                &hayro::InterpreterSettings::default(),
                &hayro::RenderSettings {
                    x_scale: scale,
                    y_scale: scale,
                    width: Some(width),
                    height: Some(height),
                },
            )
            .take_png()
        }))
        .map_err(|_| "PDF page renderer failed safely".to_string())?;
        if png.len() <= MAX_PNG_BYTES || scale <= 0.25 {
            if png.len() > MAX_PNG_BYTES {
                return Err("rendered PDF page exceeds the 4 MiB response limit".into());
            }
            break (png, width, height);
        }
        scale *= 0.65;
    };

    let page_text = pdf_extract::extract_text_from_mem_by_pages(bytes)
        .ok()
        .and_then(|pages| pages.get(pageno).cloned())
        .unwrap_or_default();
    Ok(PdfPageCapture {
        png,
        width,
        height,
        page_count,
        page_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_extract::content::{Content, Operation};
    use pdf_extract::{dictionary, Document, Object, Stream};

    fn one_page_pdf() -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal("Vellum PDF")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            content.encode().expect("content"),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).expect("save PDF");
        bytes
    }

    #[test]
    fn rejects_non_pdf_bytes() {
        assert!(extract_pdf_text(b"not a pdf").is_err());
        assert!(render_pdf_page(b"not a pdf", 0).is_err());
    }

    #[test]
    fn malformed_pdf_fails_instead_of_returning_placeholder_text() {
        let bytes = b"%PDF-1.4\n/Type /Page\n";
        assert!(extract_pdf_text(bytes).is_err());
        assert!(render_pdf_page(bytes, 0).is_err());
    }

    #[test]
    fn extracts_text_and_renders_a_real_pdf_page() {
        let bytes = one_page_pdf();
        let text = extract_pdf_text(&bytes).expect("extract text");
        assert!(text.contains("Vellum PDF"));

        let capture = render_pdf_page(&bytes, 0).expect("render page");
        assert_eq!(capture.page_count, 1);
        assert!(capture.width > 0);
        assert!(capture.height > 0);
        assert!(capture.png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
        assert!(capture.page_text.contains("Vellum PDF"));
        assert!(render_pdf_page(&bytes, 1).is_err());
    }
}
