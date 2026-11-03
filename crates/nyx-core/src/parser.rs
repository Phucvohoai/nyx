use scraper::{ElementRef, Html, Selector};
use std::collections::HashSet;
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    pub content: String,
    pub links: Vec<String>,
    pub images: Vec<ArticleImage>,
    pub forms: Vec<HtmlForm>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleImage {
    pub url: String,
    pub alt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldKind {
    Text,
    Password,
    Hidden,
    Choice,
    Checkbox,
    Radio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormField {
    pub name: String,
    pub label: String,
    pub value: String,
    pub kind: FormFieldKind,
    pub required: bool,
    pub choices: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlForm {
    pub action: String,
    pub method: FormMethod,
    pub fields: Vec<FormField>,
    pub submit_label: String,
}

#[derive(Debug, Error)]
pub enum ParserError {
    #[error("invalid HTML selector: {0}")]
    InvalidSelector(String),
    #[error("invalid page URL: {0}")]
    InvalidBaseUrl(String),
}

pub struct HtmlParser;

impl HtmlParser {
    pub fn parse_article(html: &str) -> Result<Article, ParserError> {
        Self::parse_article_at(html, "https://nyx.invalid/")
    }

    pub fn parse_article_at(html: &str, page_url: &str) -> Result<Article, ParserError> {
        let page_url =
            Url::parse(page_url).map_err(|error| ParserError::InvalidBaseUrl(error.to_string()))?;
        let document = Html::parse_document(html);
        let title_selector = selector("title")?;
        let block_selector = selector("h1, h2, h3, h4, h5, h6, p, li, blockquote, pre, td, th")?;
        let link_selector = selector("a[href]")?;
        let image_selector = selector("img[src], img[data-src]")?;
        let form_selector = selector("form")?;

        let title = document
            .select(&title_selector)
            .next()
            .map(|node| normalize_text(node.text()))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Untitled article".to_owned());

        let mut blocks = Vec::new();
        for element in document.select(&block_selector) {
            let parts = clean_text(element);
            let text = normalize_text(parts.iter().map(String::as_str));
            if !text.is_empty() && blocks.last() != Some(&text) {
                blocks.push(text);
            }
        }
        if blocks.is_empty() {
            if let Some(body) = document.select(&selector("body")?).next() {
                let parts = clean_text(body);
                let text = normalize_text(parts.iter().map(String::as_str));
                if !text.is_empty() {
                    blocks.push(text);
                }
            }
        }

        let links: Vec<String> = document
            .select(&link_selector)
            .filter(|anchor| !is_in_excluded_context(*anchor))
            .filter_map(|anchor| anchor.value().attr("href"))
            .map(str::trim)
            .filter(|href| {
                !href.is_empty()
                    && !href.to_ascii_lowercase().starts_with("javascript:")
                    && !href.to_ascii_lowercase().starts_with("data:")
            })
            .filter_map(|href| page_url.join(href).ok())
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .map(|url| url.into())
            .collect();

        let images = document
            .select(&image_selector)
            .filter(|image| !is_in_excluded_context(*image))
            .filter_map(|image| {
                let source = image
                    .value()
                    .attr("data-src")
                    .or_else(|| image.value().attr("src"))?
                    .trim();
                if source.is_empty() || source.to_ascii_lowercase().starts_with("data:") {
                    return None;
                }
                let url = page_url.join(source).ok()?;
                if !matches!(url.scheme(), "http" | "https") {
                    return None;
                }
                Some(ArticleImage {
                    url: url.into(),
                    alt: normalize_text(image.value().attr("alt").into_iter()),
                })
            })
            .take(100)
            .collect::<Vec<_>>();

        let forms: Vec<HtmlForm> = document
            .select(&form_selector)
            .filter_map(|form| parse_form(form, &page_url))
            .collect();

        let mut content = blocks.join("\n\n");
        if !forms.is_empty() {
            let references = forms
                .iter()
                .enumerate()
                .map(|(index, form)| format!("[FORM {}] {}", index + 1, form.submit_label))
                .collect::<Vec<_>>()
                .join("\n");
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&references);
        }
        if !images.is_empty() {
            let references = images
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    let description = if image.alt.is_empty() {
                        image.url.as_str()
                    } else {
                        image.alt.as_str()
                    };
                    format!("[IMAGE {}] {description}", index + 1)
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&references);
        }
        if !links.is_empty() {
            let references = links
                .iter()
                .enumerate()
                .map(|(index, href)| format!("[LINK {}] {}", index + 1, href))
                .collect::<Vec<_>>()
                .join("\n");
            if !content.is_empty() {
                content.push_str("\n\n");
            }
            content.push_str(&references);
        }

        Ok(Article {
            title,
            content,
            links,
            images,
            forms,
        })
    }
}

fn selector(value: &str) -> Result<Selector, ParserError> {
    Selector::parse(value).map_err(|error| ParserError::InvalidSelector(error.to_string()))
}

fn parse_form(element: ElementRef<'_>, page_url: &Url) -> Option<HtmlForm> {
    let action = element.value().attr("action").unwrap_or(page_url.as_str());
    let action = page_url.join(action).ok()?;
    if !matches!(action.scheme(), "http" | "https") {
        return None;
    }
    let method = match element
        .value()
        .attr("method")
        .unwrap_or("get")
        .to_ascii_lowercase()
        .as_str()
    {
        "post" => FormMethod::Post,
        "get" => FormMethod::Get,
        _ => return None,
    };

    let mut fields = Vec::new();
    let mut radio_groups = HashSet::new();
    let radio_selector = Selector::parse("input[name]").ok()?;
    let input_selector = Selector::parse("input[name], textarea[name], select[name]").ok()?;
    for input in element.select(&input_selector) {
        if input.value().attr("disabled").is_some() {
            continue;
        }
        let name = input.value().attr("name")?.to_owned();
        let tag = input.value().name();
        let input_type = input
            .value()
            .attr("type")
            .unwrap_or(if tag == "input" { "text" } else { tag })
            .to_ascii_lowercase();

        if matches!(
            input_type.as_str(),
            "submit" | "button" | "reset" | "image" | "file"
        ) {
            continue;
        }
        if input_type == "radio" && !radio_groups.insert(name.clone()) {
            continue;
        }

        let kind = match input_type.as_str() {
            "hidden" => FormFieldKind::Hidden,
            "password" => FormFieldKind::Password,
            "checkbox" => FormFieldKind::Checkbox,
            "radio" => FormFieldKind::Radio,
            "select" => FormFieldKind::Choice,
            _ => FormFieldKind::Text,
        };
        let choices = if tag == "select" {
            let option_selector = Selector::parse("option").ok()?;
            input
                .select(&option_selector)
                .filter(|option| option.value().attr("disabled").is_none())
                .map(|option| {
                    let value = option
                        .value()
                        .attr("value")
                        .map(str::to_owned)
                        .unwrap_or_else(|| normalize_text(option.text()));
                    let label = normalize_text(option.text());
                    (value, label)
                })
                .collect::<Vec<_>>()
        } else if input_type == "checkbox" {
            vec![
                (
                    input.value().attr("value").unwrap_or("on").to_owned(),
                    "checked".to_owned(),
                ),
                (String::new(), "unchecked".to_owned()),
            ]
        } else if input_type == "radio" {
            element
                .select(&radio_selector)
                .filter(|radio| radio.value().attr("name") == Some(name.as_str()))
                .filter(|radio| {
                    radio
                        .value()
                        .attr("type")
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("radio"))
                })
                .filter(|radio| radio.value().attr("disabled").is_none())
                .map(|radio| {
                    let value = radio.value().attr("value").unwrap_or("on").to_owned();
                    let label = radio
                        .ancestors()
                        .filter_map(ElementRef::wrap)
                        .find(|ancestor| ancestor.value().name() == "label")
                        .map(|label| normalize_text(label.text()))
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| value.clone());
                    (value, label)
                })
                .collect()
        } else {
            Vec::new()
        };
        let value = match kind {
            FormFieldKind::Password => String::new(),
            FormFieldKind::Choice if tag == "select" => input
                .select(&Selector::parse("option").ok()?)
                .find(|option| option.value().attr("selected").is_some())
                .map(|option| {
                    option
                        .value()
                        .attr("value")
                        .map(str::to_owned)
                        .unwrap_or_else(|| normalize_text(option.text()))
                })
                .or_else(|| choices.first().map(|(value, _)| value.clone()))
                .unwrap_or_default(),
            FormFieldKind::Radio => element
                .select(&radio_selector)
                .find(|radio| {
                    radio.value().attr("name") == Some(name.as_str())
                        && radio
                            .value()
                            .attr("type")
                            .is_some_and(|kind| kind.eq_ignore_ascii_case("radio"))
                        && radio.value().attr("checked").is_some()
                })
                .map(|radio| radio.value().attr("value").unwrap_or("on").to_owned())
                .unwrap_or_default(),
            FormFieldKind::Checkbox => {
                if input.value().attr("checked").is_some() {
                    input.value().attr("value").unwrap_or("on").to_owned()
                } else {
                    String::new()
                }
            }
            FormFieldKind::Text if tag == "textarea" => normalize_text(input.text()),
            _ => input.value().attr("value").unwrap_or_default().to_owned(),
        };
        let wrapped_label = input
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|ancestor| ancestor.value().name() == "label")
            .map(|label| normalize_text(label.text()));
        let label = wrapped_label
            .as_deref()
            .filter(|label| !label.is_empty())
            .or_else(|| {
                input
                    .value()
                    .attr("aria-label")
                    .or_else(|| input.value().attr("placeholder"))
                    .or_else(|| input.value().attr("autocomplete"))
            })
            .unwrap_or(&name)
            .to_owned();
        let required = if input_type == "radio" {
            element.select(&radio_selector).any(|radio| {
                radio.value().attr("name") == Some(name.as_str())
                    && radio
                        .value()
                        .attr("type")
                        .is_some_and(|kind| kind.eq_ignore_ascii_case("radio"))
                    && radio.value().attr("required").is_some()
            })
        } else {
            input.value().attr("required").is_some()
        };
        fields.push(FormField {
            name,
            label,
            value,
            kind,
            required,
            choices,
        });
    }

    if fields.is_empty() {
        return None;
    }
    let submit_label = element
        .select(&Selector::parse("button[type=submit], input[type=submit]").ok()?)
        .next()
        .map(|submit| {
            submit
                .value()
                .attr("value")
                .map(str::to_owned)
                .unwrap_or_else(|| normalize_text(submit.text()))
        })
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| "Submit".to_owned());

    Some(HtmlForm {
        action: action.into(),
        method,
        fields,
        submit_label,
    })
}

fn normalize_text<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn clean_text(element: ElementRef<'_>) -> Vec<String> {
    element
        .descendants()
        .filter_map(|node| {
            let text = node.value().as_text()?;
            let node_is_excluded = node
                .ancestors()
                .filter_map(ElementRef::wrap)
                .any(is_excluded);
            (!node_is_excluded).then(|| text.text.to_string())
        })
        .collect()
}

fn is_excluded(element: ElementRef<'_>) -> bool {
    matches!(
        element.value().name(),
        "script"
            | "style"
            | "noscript"
            | "iframe"
            | "svg"
            | "canvas"
            | "template"
            | "nav"
            | "aside"
            | "footer"
            | "form"
    ) || element.attr("hidden").is_some()
        || element.attr("aria-hidden") == Some("true")
}

fn is_in_excluded_context(element: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(is_excluded)
}

#[cfg(test)]
mod tests {
    use super::{FormFieldKind, FormMethod, HtmlParser};

    #[test]
    fn strips_active_and_hidden_content_and_formats_links() {
        let html = r#"
            <html><head><title> Example Article </title>
            <style>.hidden { display: none; }</style></head>
            <body><nav><p>Navigation noise</p><a href="/menu">Menu</a></nav>
            <main><h1>Heading</h1>
              <p>Readable <a href="https://example.test/page">article</a>
                 <script>tracking()</script> text.</p>
              <p hidden>Hidden text</p>
              <a href="javascript:alert(1)">unsafe</a>
            </main></body></html>
        "#;

        let article = match HtmlParser::parse_article(html) {
            Ok(article) => article,
            Err(error) => panic!("static selectors failed to parse: {error}"),
        };

        assert_eq!(article.title, "Example Article");
        assert!(article.content.contains("Heading"));
        assert!(article.content.contains("Readable article text."));
        assert!(article
            .content
            .contains("[LINK 1] https://example.test/page"));
        assert!(!article.content.contains("tracking"));
        assert!(!article.content.contains("Navigation noise"));
        assert!(!article.content.contains("Hidden text"));
        assert_eq!(article.links, ["https://example.test/page"]);
    }

    #[test]
    fn parses_standard_login_form_and_resolves_its_action() {
        let html = r#"
            <title>Sign in</title>
            <form action="/session" method="post">
              <input type="hidden" name="csrf" value="token123">
              <input type="email" name="email" placeholder="Email" required>
              <input type="password" name="password" placeholder="Password" required>
              <button type="submit">Sign in</button>
            </form>
        "#;
        let article = match HtmlParser::parse_article_at(html, "https://example.test/login") {
            Ok(article) => article,
            Err(error) => panic!("login form failed to parse: {error}"),
        };

        assert_eq!(article.forms.len(), 1);
        assert_eq!(article.forms[0].action, "https://example.test/session");
        assert_eq!(article.forms[0].method, FormMethod::Post);
        assert_eq!(article.forms[0].submit_label, "Sign in");
        assert_eq!(article.forms[0].fields[0].kind, FormFieldKind::Hidden);
        assert_eq!(article.forms[0].fields[0].value, "token123");
        assert_eq!(article.forms[0].fields[1].label, "Email");
        assert!(article.forms[0].fields[1].required);
        assert_eq!(article.forms[0].fields[2].kind, FormFieldKind::Password);
        assert!(article.forms[0].fields[2].value.is_empty());
        assert!(article.content.contains("[FORM 1] Sign in"));
        assert!(!article.content.contains("token123"));
    }

    #[test]
    fn groups_radio_buttons_and_keeps_unchecked_checkboxes_editable() {
        let html = r#"
            <form>
              <label><input type="radio" name="mode" value="basic" required>Basic</label>
              <label><input type="radio" name="mode" value="advanced">Advanced</label>
              <input type="checkbox" name="remember" value="yes">
              <input type="checkbox" name="updates" value="yes" checked>
            </form>
        "#;
        let article = match HtmlParser::parse_article_at(html, "https://example.test/") {
            Ok(article) => article,
            Err(error) => panic!("choice form failed to parse: {error}"),
        };
        let fields = &article.forms[0].fields;

        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0].kind, FormFieldKind::Radio);
        assert!(fields[0].required);
        assert!(fields[0].value.is_empty());
        assert_eq!(fields[0].choices.len(), 2);
        assert_eq!(fields[1].kind, FormFieldKind::Checkbox);
        assert!(fields[1].value.is_empty());
        assert_eq!(fields[2].value, "yes");
    }

    #[test]
    fn resolves_web_images_and_ignores_data_and_script_images() {
        let article = match HtmlParser::parse_article_at(
            r#"
                <html><body>
                  <p>Comic page</p>
                  <img src="/pages/1.jpg" alt="Page one">
                  <img data-src="/pages/2.webp" alt="Page two">
                  <img src="data:image/png;base64,AAAA" alt="Inline">
                  <script><img src="https://tracking.test/pixel.png"></script>
                </body></html>
            "#,
            "https://comics.example/series/chapter-1",
        ) {
            Ok(article) => article,
            Err(error) => panic!("article image parsing failed: {error}"),
        };

        assert_eq!(article.images.len(), 2);
        assert_eq!(article.images[0].url, "https://comics.example/pages/1.jpg");
        assert_eq!(article.images[0].alt, "Page one");
        assert_eq!(article.images[1].url, "https://comics.example/pages/2.webp");
        assert_eq!(article.images[1].alt, "Page two");
        assert!(article.content.contains("[IMAGE 1] Page one"));
        assert!(article.content.contains("[IMAGE 2] Page two"));
    }
}
