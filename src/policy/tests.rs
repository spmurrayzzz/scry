use super::*;

#[test]
fn url_syntax_and_port_guards_preserve_public_url_meaning() {
    let limits = Limits::default();
    for (input, expected) in [
        (
            "HTTPS://DOCS.RS.:443/reqwest/../reqwest/?q=a%20b#method",
            "https://docs.rs/reqwest/?q=a%20b",
        ),
        ("http://8.8.8.8:80/a#x", "http://8.8.8.8/a"),
        (
            "https://[2606:4700:4700::1111]:443/",
            "https://[2606:4700:4700::1111]/",
        ),
        ("https://bücher.de/", "https://xn--bcher-kva.de/"),
    ] {
        assert_eq!(parse_url(input, &limits).unwrap().as_str(), expected);
    }
    for input in [
        "https://docs.rs:80/",
        "http://8.8.8.8:443/",
        "https://[2606:4700:4700::1111]:8080/",
    ] {
        assert_eq!(
            parse_url(input, &limits).unwrap_err().code,
            ErrorCode::BlockedDestination,
            "{input}"
        );
    }
    for input in [
        "",
        "file:///etc/passwd",
        "ftp://docs.rs/",
        "https:docs.rs",
        "https:///docs.rs/",
        "https://user:pass@docs.rs/",
        "https://@docs.rs/",
        "https://docs.rs\\other/",
        "https://docs.rs/a b",
        "https://docs.rs/\npath",
        "https://bad_label.org/",
        "https://-bad.org/",
    ] {
        assert_eq!(
            parse_url(input, &limits).unwrap_err().code,
            ErrorCode::InvalidInput,
            "{input:?}"
        );
    }
}

#[test]
fn url_policy_blocks_private_and_special_hosts_on_standard_ports() {
    let limits = Limits::default();
    for host in [
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f000001",
        "10.0.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "[::ffff:8.8.8.8]",
        "[fd7a:115c:a1e0::1]",
        "localhost",
        "service.localhost.",
        "host.local",
        "host.internal",
        "host.test",
        "host.invalid",
        "host.example",
        "host.onion",
        "host.arpa",
        "host.ts.net",
        "metadata.google.com",
        "metadata.goog",
        "singlelabel",
    ] {
        for (scheme, port) in [("http", 80), ("https", 443)] {
            let input = format!("{scheme}://{host}:{port}/");
            assert_eq!(
                parse_url(&input, &limits).unwrap_err().code,
                ErrorCode::BlockedDestination,
                "{input}"
            );
        }
    }
    for input in [
        "https://docs.rs/",
        "https://notmetadata.google.com/",
        "https://ts.net.evil.org/",
        "https://8.8.8.8/",
        "http://[2606:4700:4700::1111]/",
    ] {
        assert!(parse_url(input, &limits).is_ok(), "{input}");
    }
}

#[test]
fn ip_policy_rejects_special_ranges_and_keeps_public_boundary_controls() {
    for input in [
        "0.1.2.3",
        "10.255.255.255",
        "100.64.0.0",
        "100.127.255.255",
        "127.255.255.255",
        "169.254.0.1",
        "172.16.0.0",
        "172.31.255.255",
        "192.0.0.9",
        "192.0.2.1",
        "192.31.196.1",
        "192.52.193.1",
        "192.88.99.1",
        "192.168.1.1",
        "192.175.48.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "168.63.129.16",
        "::",
        "::1",
        "::ffff:8.8.8.8",
        "::ffff:10.0.0.1",
        "64:ff9b::808:808",
        "100::1",
        "2001::1",
        "2001:db8::1",
        "2002:808:808::1",
        "2620:4f:8000::1",
        "fc00::1",
        "fd7a:115c:a1e0::1",
        "fe80::1",
        "ff02::1",
    ] {
        assert_eq!(
            validate_ip(input.parse().unwrap()).unwrap_err().code,
            ErrorCode::BlockedDestination,
            "{input}"
        );
    }
    for input in [
        "8.8.8.8",
        "1.1.1.1",
        "100.63.255.255",
        "100.128.0.0",
        "172.15.255.255",
        "172.32.0.0",
        "198.17.255.255",
        "198.20.0.0",
        "168.63.129.15",
        "2606:4700:4700::1111",
        "2001:4860:4860::8888",
        "2620:fe::fe",
        "2a00:1450:4001::1",
    ] {
        assert!(validate_ip(input.parse().unwrap()).is_ok(), "{input}");
    }
}

#[test]
fn domain_filters_use_label_boundaries_idna_and_exclude_precedence() {
    let limits = Limits::default();
    let filter = DomainFilter::new(
        &["DOCS.RS.".into(), "BÜCHER.DE".into(), "8.8.8.8".into()],
        &["private.docs.rs".into()],
        &limits,
    )
    .unwrap();
    for input in [
        "https://docs.rs/",
        "https://sub.docs.rs/",
        "https://xn--bcher-kva.de/",
        "https://sub.bücher.de/",
        "https://8.8.8.8/",
    ] {
        assert!(filter.check(&Url::parse(input).unwrap()).is_ok(), "{input}");
    }
    for input in [
        "https://notdocs.rs/",
        "https://docs.rs.evil.org/",
        "https://private.docs.rs/",
        "https://child.private.docs.rs/",
        "https://8.8.4.4/",
    ] {
        assert_eq!(
            filter.check(&Url::parse(input).unwrap()).unwrap_err().code,
            ErrorCode::DomainFiltered,
            "{input}"
        );
    }
    let excluded = DomainFilter::new(&["docs.rs".into()], &["docs.rs".into()], &limits).unwrap();
    assert_eq!(
        excluded
            .check(&Url::parse("https://docs.rs/").unwrap())
            .unwrap_err()
            .code,
        ErrorCode::DomainFiltered
    );
    let ipv6 = DomainFilter::new(&["[2606:4700:4700::1111]".into()], &[], &limits).unwrap();
    assert!(
        ipv6.check(&Url::parse("https://[2606:4700:4700::1111]/").unwrap())
            .is_ok()
    );
    for input in [
        "https://docs.rs",
        "docs.rs:443",
        "*.docs.rs",
        "docs.rs/path",
        "",
    ] {
        assert!(
            matches!(DomainFilter::new(&[input.into()], &[], &limits), Err(error) if error.code == ErrorCode::InvalidInput),
            "{input}"
        );
    }
}
