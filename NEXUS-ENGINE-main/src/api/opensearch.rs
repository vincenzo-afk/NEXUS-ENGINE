/// Generate the OpenSearch 1.1 XML description document.
pub fn generate_opensearch_xml(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OpenSearchDescription xmlns="http://a9.com/-/spec/opensearch/1.1/">
  <ShortName>Nexus</ShortName>
  <Description>Privacy-first universal search engine</Description>
  <InputEncoding>UTF-8</InputEncoding>
  <OutputEncoding>UTF-8</OutputEncoding>
  <Url type="text/html" method="get" template="{base}/search?q={{searchTerms}}"/>
  <Url type="application/x-suggestions+json" method="get" template="{base}/suggest?prefix={{searchTerms}}"/>
  <Image height="16" width="16" type="image/x-icon">{base}/favicon.ico</Image>
  <Language>en-US</Language>
  <Attribution>Search results are private and not tracked</Attribution>
</OpenSearchDescription>"#,
        base = base
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_starts_with_declaration() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.starts_with("<?xml"));
    }

    #[test]
    fn contains_short_name() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("<ShortName>Nexus</ShortName>"));
    }

    #[test]
    fn contains_description() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("<Description>Privacy-first universal search engine</Description>"));
    }

    #[test]
    fn contains_search_url_template() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("http://localhost:8080/search?q={searchTerms}"));
    }

    #[test]
    fn contains_suggestions_url() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("http://localhost:8080/suggest?prefix={searchTerms}"));
    }

    #[test]
    fn contains_favicon_image() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("http://localhost:8080/favicon.ico"));
        assert!(xml.contains("<Image"));
    }

    #[test]
    fn has_opensearch_namespace() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("xmlns=\"http://a9.com/-/spec/opensearch/1.1/\""));
    }

    #[test]
    fn handles_trailing_slash() {
        let xml = generate_opensearch_xml("http://localhost:8080/");
        assert!(xml.contains("http://localhost:8080/search?q={searchTerms}"));
    }

    #[test]
    fn includes_encodings() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("<InputEncoding>UTF-8</InputEncoding>"));
        assert!(xml.contains("<OutputEncoding>UTF-8</OutputEncoding>"));
    }

    #[test]
    fn includes_attribution() {
        let xml = generate_opensearch_xml("http://localhost:8080");
        assert!(xml.contains("<Attribution>"));
    }
}
