use file_format::FileFormat;
use liteparse::types::PdfInput;
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;
use crate::extract::{ExtractedPage, extract_plain};

#[cfg(test)]
mod tests;

pub(crate) fn extract(
    bytes: &[u8],
    query: Option<&str>,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<ExtractedPage, Error> {
    ctx.check()?;
    if FileFormat::from_bytes(bytes) != FileFormat::PortableDocumentFormat {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Page is not a PDF document",
        ));
    }
    let config = LiteParseConfig {
        ocr_enabled: false,
        max_pages: limits.max_pdf_pages,
        output_format: OutputFormat::Text,
        quiet: true,
        ..LiteParseConfig::default()
    };
    let parser = LiteParse::new(config);
    let result = tokio::runtime::Handle::current()
        .block_on(parser.parse_input(PdfInput::Bytes(bytes.to_vec())))
        .map_err(|_| {
            Error::new(
                ErrorCode::UnsupportedContent,
                "Page is not a readable PDF document",
            )
        })?;
    ctx.check()?;
    let mut page = extract_plain(&result.text, query, ctx, limits)?;
    page.truncated |= result.total_pages > result.pages.len() as u32;
    Ok(page)
}
