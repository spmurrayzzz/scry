use pulldown_cmark::{Event, Parser};
use scry::{Config, ContentOrigin, ContentsRequest, ItemStatus, Scry, SearchRequest};
use url::Url;

#[tokio::test]
#[ignore = "makes one production DuckDuckGo search and reads one returned documentation page"]
async fn production_search_and_contents_return_useful_reqwest_client_builder_documentation()
-> Result<(), Box<dyn std::error::Error>> {
    let scry = Scry::new(Config::default())?;
    let search = scry
        .search(SearchRequest {
            query: "reqwest ClientBuilder documentation".into(),
            include_domains: vec!["docs.rs".into()],
            ..SearchRequest::default()
        })
        .await?;
    let source = search
        .results
        .iter()
        .find(|result| {
            Url::parse(&result.url).is_ok_and(|url| {
                url.host_str() == Some("docs.rs")
                    && url.path().contains("/reqwest/")
                    && url.path().ends_with("/struct.ClientBuilder.html")
            })
        })
        .expect("production search did not return reqwest ClientBuilder documentation");
    assert_eq!(source.status, ItemStatus::Ok);
    assert!(source.error.is_none());
    assert_eq!(source.id, source.url);
    assert!(Url::parse(&source.id)?.fragment().is_none());
    assert!(
        source
            .title
            .as_deref()
            .is_some_and(|title| title.contains("ClientBuilder"))
    );
    assert_ne!(source.content_origin, Some(ContentOrigin::Live));
    assert!(source.text.is_none());
    assert!(source.final_url.is_none());
    let contents = scry
        .contents(ContentsRequest {
            ids: vec![source.id.clone()],
            ..ContentsRequest::default()
        })
        .await?;
    assert_eq!(contents.results.len(), 1);
    let page = &contents.results[0];
    assert_eq!(page.status, ItemStatus::Ok, "{page:?}");
    assert!(page.error.is_none());
    assert_eq!(page.id, source.id);
    assert_eq!(page.url, source.url);
    assert_eq!(page.content_origin, Some(ContentOrigin::Live));
    let final_url = Url::parse(
        page.final_url
            .as_deref()
            .expect("live page has no final URL"),
    )?;
    assert_eq!(final_url.host_str(), Some("docs.rs"));
    assert_eq!(final_url.scheme(), "https");
    assert!(final_url.path().contains("/reqwest/"));
    assert!(final_url.path().ends_with("/struct.ClientBuilder.html"));
    let text = page
        .text
        .as_deref()
        .expect("live page has no extracted text");
    let visible = Parser::new(text)
        .filter_map(|event| match event {
            Event::Text(text) | Event::Code(text) => Some(text.into_string()),
            Event::SoftBreak | Event::HardBreak => Some(" ".to_owned()),
            _ => None,
        })
        .collect::<String>();
    assert!(
        visible
            .split_whitespace()
            .collect::<String>()
            .contains("pubfnbuild(self)->Result<Client>"),
        "documentation is missing the ClientBuilder::build signature: {page:?}"
    );
    assert!(
        visible
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("Returns a Client that uses this ClientBuilder configuration."),
        "documentation is missing the ClientBuilder::build explanation: {page:?}"
    );
    Ok(())
}
