use crate::api::{Error, ErrorCode};

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub limits: Limits,
}

impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        self.limits.validate()
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_query_chars: usize,
    pub max_urls: usize,
    pub max_domain_filters: usize,
    pub max_url_bytes: usize,
    pub max_request_bytes: usize,
    pub max_download_bytes: usize,
    pub max_decoded_bytes: usize,
    pub max_text_chars: usize,
    pub max_pdf_pages: usize,
    pub max_response_bytes: usize,
    pub max_redirects: usize,
    pub max_html_tokens: usize,
    pub max_html_depth: usize,
    pub max_requests: usize,
    pub max_fetches: usize,
    pub max_parsers: usize,
    pub request_timeout_secs: u64,
    pub fetch_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub dns_timeout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_query_chars: 499,
            max_urls: 20,
            max_domain_filters: 20,
            max_url_bytes: 8192,
            max_request_bytes: 65536,
            max_download_bytes: 2097152,
            max_decoded_bytes: 4194304,
            max_text_chars: 50000,
            max_pdf_pages: 200,
            max_response_bytes: 1048576,
            max_redirects: 5,
            max_html_tokens: 100000,
            max_html_depth: 128,
            max_requests: 20,
            max_fetches: 8,
            max_parsers: 2,
            request_timeout_secs: 30,
            fetch_timeout_secs: 15,
            connect_timeout_secs: 5,
            dns_timeout_secs: 3,
        }
    }
}

impl Limits {
    pub fn validate(&self) -> Result<(), Error> {
        for (name, value, maximum) in [
            ("max_query_chars", self.max_query_chars, 16384),
            ("max_urls", self.max_urls, 100),
            ("max_domain_filters", self.max_domain_filters, 100),
            ("max_url_bytes", self.max_url_bytes, 65536),
            ("max_request_bytes", self.max_request_bytes, 1048576),
            ("max_download_bytes", self.max_download_bytes, 67108864),
            ("max_decoded_bytes", self.max_decoded_bytes, 134217728),
            ("max_text_chars", self.max_text_chars, 1000000),
            ("max_pdf_pages", self.max_pdf_pages, 5000),
            ("max_response_bytes", self.max_response_bytes, 16777216),
            ("max_redirects", self.max_redirects, 20),
            ("max_html_tokens", self.max_html_tokens, 2000000),
            ("max_html_depth", self.max_html_depth, 1024),
            ("max_requests", self.max_requests, 1024),
            ("max_fetches", self.max_fetches, 1024),
            ("max_parsers", self.max_parsers, 128),
        ] {
            if value == 0 || value > maximum {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("{name} must be between 1 and {maximum}"),
                ));
            }
        }
        for (name, value, maximum) in [
            ("request_timeout_secs", self.request_timeout_secs, 300),
            ("fetch_timeout_secs", self.fetch_timeout_secs, 120),
            ("connect_timeout_secs", self.connect_timeout_secs, 30),
            ("dns_timeout_secs", self.dns_timeout_secs, 30),
        ] {
            if value == 0 || value > maximum {
                return Err(Error::new(
                    ErrorCode::InvalidInput,
                    format!("{name} must be between 1 and {maximum}"),
                ));
            }
        }
        Ok(())
    }
}
