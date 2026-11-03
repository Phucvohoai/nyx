use scraper::{ElementRef, Html, Selector};
use thiserror::Error;
use url::Url;

use crate::SearchResult;

const DUCKDUCKGO_HTML: &str = "https://html.duckduckgo.com/html/";
const RESULT_LIMIT: usize = 50;
const SNIPPET_LIMIT: usize = 280;
const DEFAULT_SEARXNG_ENDPOINT: &str = "http://127.0.0.1:8080/search";

pub fn duckduckgo_search_endpoint() -> &'static str {
    DUCKDUCKGO_HTML
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid search result selector: {0}")]
    InvalidSelector(String),
    #[error("invalid search URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("invalid SearXNG endpoint: {0}")]
    InvalidSearxngEndpoint(String),
    #[error("invalid SearXNG JSON response: {0}")]
    InvalidSearxngJson(String),
    #[error("search provider requested human verification")]
    ProviderChallenge,
}

pub fn default_searxng_endpoint() -> &'static str {
    DEFAULT_SEARXNG_ENDPOINT
}

pub fn searxng_search_url(endpoint: &str, query: &str) -> Result<String, SearchError> {
    let mut url = Url::parse(endpoint)
        .map_err(|error| SearchError::InvalidSearxngEndpoint(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(SearchError::InvalidSearxngEndpoint(
            "expected an absolute HTTP or HTTPS /search endpoint".to_owned(),
        ));
    }

    let existing_pairs = url
        .query_pairs()
        .filter(|(key, _)| key != "format" && key != "q")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    {
        let mut pairs = url.query_pairs_mut();
        pairs.clear().extend_pairs(existing_pairs);
        pairs.append_pair("format", "json").append_pair("q", query);
    }
    Ok(url.into())
}

pub fn parse_searxng_results(json: &str) -> Result<Vec<SearchResult>, SearchError> {
    let document: serde_json::Value = serde_json::from_str(json)
        .map_err(|error| SearchError::InvalidSearxngJson(error.to_string()))?;
    let results = document
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SearchError::InvalidSearxngJson("missing results array".to_owned()))?;

    let mut parsed = Vec::new();
    for result in results {
        let Some(title) = result.get("title").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(raw_url) = result.get("url").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Ok(url) = Url::parse(raw_url) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https")
            || title.trim().is_empty()
            || parsed
                .iter()
                .any(|entry: &SearchResult| entry.url == url.as_str())
        {
            continue;
        }
        let snippet = result
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        parsed.push(SearchResult {
            title: title.trim().to_owned(),
            url: url.into(),
            snippet: truncate(snippet, SNIPPET_LIMIT),
        });
        if parsed.len() == RESULT_LIMIT {
            break;
        }
    }
    if !results.is_empty() && parsed.is_empty() {
        return Err(SearchError::InvalidSearxngJson(
            "results array contained no valid HTTP(S) result URLs".to_owned(),
        ));
    }
    Ok(parsed)
}

pub fn duckduckgo_search_url(query: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    format!("{DUCKDUCKGO_HTML}?{query}")
}

pub fn parse_duckduckgo_results(html: &str) -> Result<Vec<SearchResult>, SearchError> {
    let document = Html::parse_document(html);
    let result_link_selector = selector("a.result__a, a.result-link")?;
    let snippet_selector = selector(".result__snippet, .result-snippet")?;
    if is_challenge_page(&document)? {
        return Err(SearchError::ProviderChallenge);
    }

    let base_url = Url::parse(DUCKDUCKGO_HTML)?;
    let mut results = Vec::new();

    for anchor in document.select(&result_link_selector) {
        let title = normalize_text(anchor.text());
        let Some(href) = anchor.value().attr("href") else {
            continue;
        };
        let Some(url) = resolve_result_url(href, &base_url)? else {
            continue;
        };
        if title.is_empty()
            || results
                .iter()
                .any(|result: &SearchResult| result.url == url)
        {
            continue;
        }

        let snippet = anchor
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|ancestor| {
                ancestor.value().attr("class").is_some_and(|classes| {
                    classes.split_whitespace().any(|class| class == "result")
                })
            })
            .and_then(|result| result.select(&snippet_selector).next())
            .map(|element| normalize_text(element.text()))
            .unwrap_or_default();

        results.push(SearchResult {
            title,
            url,
            snippet: truncate(&snippet, SNIPPET_LIMIT),
        });
        if results.len() == RESULT_LIMIT {
            break;
        }
    }

    Ok(results)
}

pub fn normalize_direct_url(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() || input.chars().any(char::is_whitespace) {
        return None;
    }
    let candidate = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let mut url = Url::parse(&candidate).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    match url.host()? {
        url::Host::Domain(domain) if !domain.contains('.') && domain != "localhost" => None,
        _ => Some(url.into()),
    }
}

fn is_challenge_page(document: &Html) -> Result<bool, SearchError> {
    let challenge_selector =
        selector("form#challenge-form, .anomaly-modal__modal, form[action*='captcha'], .captcha")?;
    if document.select(&challenge_selector).next().is_some() {
        return Ok(true);
    }

    let body_selector = selector("body")?;
    let body_text = document
        .select(&body_selector)
        .next()
        .map(|body| normalize_text(body.text()).to_ascii_lowercase())
        .unwrap_or_default();
    Ok([
        "bots use duckduckgo too",
        "human verification",
        "complete the following challenge",
        "unusual traffic",
    ]
    .iter()
    .any(|marker| body_text.contains(marker)))
}

fn selector(value: &str) -> Result<Selector, SearchError> {
    Selector::parse(value).map_err(|error| SearchError::InvalidSelector(error.to_string()))
}

fn normalize_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_result_url(href: &str, base_url: &Url) -> Result<Option<String>, SearchError> {
    let destination = base_url.join(href)?;
    let is_duckduckgo_redirect = destination
        .host_str()
        .is_some_and(|host| host == "duckduckgo.com" || host.ends_with(".duckduckgo.com"));
    let target = is_duckduckgo_redirect.then(|| {
        destination
            .query_pairs()
            .find_map(|(key, value)| (key == "uddg").then(|| value.into_owned()))
    });
    let target = target.flatten();
    let destination = match target {
        Some(target) => Url::parse(&target)?,
        None => destination,
    };

    if matches!(destination.scheme(), "http" | "https") {
        Ok(Some(destination.into()))
    } else {
        Ok(None)
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let prefix: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

pub fn rank_results(
    query: &str,
    results: impl IntoIterator<Item = SearchResult>,
) -> Vec<SearchResult> {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        return Vec::new();
    }

    let mut ranked: Vec<(usize, usize, SearchResult)> = results
        .into_iter()
        .enumerate()
        .filter_map(|(position, result)| {
            let title = result.title.to_lowercase();
            let snippet = result.snippet.to_lowercase();
            let url = result.url.to_lowercase();
            let score = terms
                .iter()
                .map(|term| {
                    if title.contains(term) {
                        3
                    } else if snippet.contains(term) {
                        2
                    } else if url.contains(term) {
                        1
                    } else {
                        0
                    }
                })
                .sum::<usize>();
            (score > 0).then_some((score, position, result))
        })
        .collect();

    ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    ranked.into_iter().map(|(_, _, result)| result).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        default_searxng_endpoint, duckduckgo_search_endpoint, duckduckgo_search_url,
        normalize_direct_url, parse_duckduckgo_results, parse_searxng_results, rank_results,
        searxng_search_url,
    };
    use crate::SearchResult;
    use url::Url;

    #[test]
    fn builds_searxng_json_request_url_and_parses_results() {
        assert_eq!(default_searxng_endpoint(), "http://127.0.0.1:8080/search");
        let url =
            match searxng_search_url("http://localhost:8080/search?token=abc", "zalo & privacy") {
                Ok(url) => url,
                Err(error) => panic!("SearXNG URL failed: {error}"),
            };
        let parsed_url = match Url::parse(&url) {
            Ok(url) => url,
            Err(error) => panic!("generated endpoint URL failed to parse: {error}"),
        };
        let pairs = parsed_url.query_pairs().collect::<Vec<_>>();
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "format" && value == "json"));
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "q" && value == "zalo & privacy"));
        assert!(pairs
            .iter()
            .any(|(key, value)| key == "token" && value == "abc"));

        let json = r#"{"results":[
          {"title":"Zalo","url":"https://zalo.me/","content":"Messaging service."},
          {"title":"Unsafe","url":"javascript:alert(1)","content":"ignore"},
          {"title":"Zalo duplicate","url":"https://zalo.me/","content":"duplicate"}
        ]}"#;
        let results = match parse_searxng_results(json) {
            Ok(results) => results,
            Err(error) => panic!("SearXNG result JSON failed to parse: {error}"),
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Zalo");
        assert_eq!(results[0].url, "https://zalo.me/");
        assert_eq!(results[0].snippet, "Messaging service.");
        assert!(parse_searxng_results(r#"{"wrong":[]}"#).is_err());
    }

    #[test]
    fn ranks_title_matches_before_snippet_matches() {
        let results = vec![
            SearchResult {
                title: "Other".to_owned(),
                url: "https://example.test/".to_owned(),
                snippet: "Nyx browser".to_owned(),
            },
            SearchResult {
                title: "Nyx browser".to_owned(),
                url: "https://example.test/nyx".to_owned(),
                snippet: "Terminal".to_owned(),
            },
        ];

        let ranked = rank_results("nyx", results);

        assert_eq!(ranked[0].title, "Nyx browser");
        assert_eq!(ranked[1].title, "Other");
    }

    #[test]
    fn encodes_query_and_parses_redirected_results() {
        let query = "rust privacy & text";
        assert_eq!(
            duckduckgo_search_endpoint(),
            "https://html.duckduckgo.com/html/"
        );
        assert!(duckduckgo_search_url(query).contains("q=rust+privacy+%26+text"));
        let html = r#"
            <div class="result">
              <h2><a class="result__a"
                href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.test%2Fpage%3Fx%3D1">
                Example &amp; Result
              </a></h2>
              <a class="result__snippet">A useful <b>text browser</b> result.</a>
            </div>
            <div class="result">
              <a class="result__a" href="javascript:alert(1)">Unsafe</a>
            </div>
        "#;

        let results = match parse_duckduckgo_results(html) {
            Ok(results) => results,
            Err(error) => panic!("search result HTML failed to parse: {error}"),
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Example & Result");
        assert_eq!(results[0].url, "https://example.test/page?x=1");
        assert_eq!(results[0].snippet, "A useful text browser result.");
    }

    #[test]
    fn reports_provider_human_verification_challenge() {
        let error = parse_duckduckgo_results(
            r#"<form id="challenge-form">            <div class="MjjYud">Please verify</div></form>"#,
        );
        assert!(matches!(error, Err(super::SearchError::ProviderChallenge)));
    }

    #[test]
    fn parses_duckduckgo_lite_result_markup() {
        let html = r#"
          <table>
            <tr class="result">
              <td><a class="result-link" href="https://zalo.me/">Zalo Official</a></td>
              <td class="result-snippet">Messaging platform.</td>
            </tr>
          </table>
        "#;
        let results = match parse_duckduckgo_results(html) {
            Ok(results) => results,
            Err(error) => panic!("DuckDuckGo Lite result HTML failed to parse: {error}"),
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Zalo Official");
        assert_eq!(results[0].url, "https://zalo.me/");
        assert_eq!(results[0].snippet, "Messaging platform.");
    }

    #[test]
    fn detects_text_based_provider_verification_pages() {
        let error = parse_duckduckgo_results(
            "<html><body>Unfortunately, bots use DuckDuckGo too. Human verification.</body></html>",
        );
        assert!(matches!(error, Err(super::SearchError::ProviderChallenge)));
    }

    #[test]
    fn normalizes_domains_and_http_urls_for_direct_navigation() {
        assert_eq!(
            normalize_direct_url("  github.com  "),
            Some("https://github.com/".to_owned())
        );
        assert_eq!(
            normalize_direct_url("example.test/path"),
            Some("https://example.test/path".to_owned())
        );
        assert_eq!(
            normalize_direct_url("http://localhost:3000"),
            Some("https://localhost:3000/".to_owned())
        );
        assert_eq!(normalize_direct_url("facebook"), None);
        assert_eq!(normalize_direct_url("search for cats"), None);
        assert_eq!(normalize_direct_url("javascript:alert(1)"), None);
        assert_eq!(normalize_direct_url("https://user:pass@example.com"), None);
    }
}
