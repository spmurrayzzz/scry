use std::time::Duration;

use lopdf::content::{Content, Operation};
use lopdf::{Document, Object, Stream, dictionary};

use super::*;
use crate::config::Limits;
use crate::context::RequestContext;

fn fixture(pages: &[&str]) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let mut kids = Vec::new();
    for text in pages {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![20.into(), 100.into()]),
                Operation::new("Tj", vec![(*text).into()]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![
                Object::Integer(0),
                Object::Integer(0),
                Object::Integer(200),
                Object::Integer(200),
            ],
            "Contents" => Object::Reference(content_id),
            "Resources" => dictionary! {
                "Font" => dictionary! { "F1" => Object::Reference(font_id) },
            },
        });
        kids.push(Object::Reference(page_id));
    }
    doc.objects.insert(
        pages_id,
        dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => Object::Integer(pages.len() as i64),
        }
        .into(),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    bytes
}

fn context() -> RequestContext {
    RequestContext::new(Duration::from_secs(30))
}

#[tokio::test]
async fn extract_reads_a_generated_fixture() {
    let bytes = fixture(&["Hello scry"]);
    let ctx = context();
    let page =
        tokio::task::spawn_blocking(move || super::extract(&bytes, None, &ctx, &Limits::default()))
            .await
            .unwrap()
            .unwrap();
    assert!(page.text.contains("Hello scry"), "{}", page.text);
    assert!(!page.truncated);
}

#[tokio::test]
async fn extract_rejects_non_pdf_bytes_before_conversion() {
    for bytes in [
        b"\x89PNG\r\n\x1a\nnot a pdf".to_vec(),
        b"PK\x03\x04 not a pdf".to_vec(),
        b"plain text pretending".to_vec(),
        b"%PDFx junk".to_vec(),
        b"%PDF-1.4\n%AIPrivateData junk".to_vec(),
    ] {
        let ctx = context();
        let error = tokio::task::spawn_blocking(move || {
            super::extract(&bytes, None, &ctx, &Limits::default())
        })
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::UnsupportedContent);
        assert_eq!(error.message, "Page is not a PDF document");
    }
}

#[tokio::test]
async fn extract_rejects_corrupted_pdf_bytes() {
    let bytes = b"%PDF-1.4 this is not a pdf".to_vec();
    let ctx = context();
    let error =
        tokio::task::spawn_blocking(move || super::extract(&bytes, None, &ctx, &Limits::default()))
            .await
            .unwrap()
            .err()
            .unwrap();
    assert_eq!(error.code, ErrorCode::UnsupportedContent);
    assert_eq!(error.message, "Page is not a readable PDF document");
}

#[tokio::test]
async fn extract_reports_page_limit_omission() {
    let bytes = fixture(&["first page", "second page"]);
    let ctx = context();
    let limits = Limits {
        max_pdf_pages: 1,
        ..Limits::default()
    };
    let page = tokio::task::spawn_blocking(move || super::extract(&bytes, None, &ctx, &limits))
        .await
        .unwrap()
        .unwrap();
    assert!(page.text.contains("first page"), "{}", page.text);
    assert!(!page.text.contains("second page"));
    assert!(page.truncated);
}

#[tokio::test]
async fn extract_rejects_documents_without_readable_text() {
    let bytes = fixture(&[""]);
    let ctx = context();
    let error =
        tokio::task::spawn_blocking(move || super::extract(&bytes, None, &ctx, &Limits::default()))
            .await
            .unwrap()
            .err()
            .unwrap();
    assert_eq!(error.code, ErrorCode::ExtractionFailed);
}
