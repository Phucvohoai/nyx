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
    pub reader: ReaderDocument,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderDocument {
    pub title: String,
    pub byline: Option<String>,
    pub blocks: Vec<ReaderBlock>,
    pub links: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderBlock {
    Heading {
        level: u8,
        content: Vec<ReaderInline>,
    },
    Paragraph(Vec<ReaderInline>),
    Blockquote(Vec<ReaderInline>),
    List {
        ordered: bool,
        items: Vec<Vec<ReaderInline>>,
    },
    LinkList(Vec<ReaderLinkItem>),
    Code(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderLinkItem {
    pub text: String,
    pub link_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderInline {
    Text(String),
    Code(String),
    Link { text: String, link_index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleImage {
    pub url: String,
    pub alt: String,
    pub referer: String,
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
        if html.starts_with("%PDF-")
            || html
                .chars()
                .take(100)
                .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
        {
            return Ok(Article {
                title: "Binary / Non-HTML Resource".to_owned(),
                content: "[INFO] Cannot render binary, PDF, or non-text stream directly."
                    .to_owned(),
                links: vec![],
                images: vec![],
                forms: vec![],
                reader: ReaderDocument {
                    title: "Binary / Non-HTML Resource".to_owned(),
                    byline: None,
                    blocks: vec![ReaderBlock::Paragraph(vec![ReaderInline::Text(
                        "[INFO] Cannot render binary, PDF, or non-text stream directly.".to_owned(),
                    )])],
                    links: vec![],
                },
            });
        }

        let page_url =
            Url::parse(page_url).map_err(|error| ParserError::InvalidBaseUrl(error.to_string()))?;
        let document = Html::parse_document(html);
        let title_selector = selector("title")?;
        let block_selector =
            selector("h1, h2, h3, h4, h5, h6, p, ul, ol, blockquote, pre, td, th")?;
        let list_item_selector = selector("li")?;
        let anchor_selector = selector("a[href]")?;
        let repeated_item_selector = selector("li, tr")?;
        let paragraph_selector = selector("p")?;
        let title_link_selector = selector(".titleline a[href]")?;
        let image_selector = selector(
            "img[src], img[data-src], img[data-srcset], img[srcset], img[data-original], img[data-lazy-src]",
        )?;
        let form_selector = selector("form")?;
        let body_selector = selector("body")?;
        let article_selector = selector("article")?;
        let main_selector = selector("main, [role=main]")?;
        let content_candidate_selector = selector("body, section, div")?;

        let title = document
            .select(&title_selector)
            .next()
            .map(|node| normalize_text(node.text()))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Untitled article".to_owned());

        let root = document
            .select(&article_selector)
            .find(|element| !is_in_excluded_context(*element))
            .or_else(|| {
                document
                    .select(&main_selector)
                    .find(|element| !is_in_excluded_context(*element))
            })
            .or_else(|| {
                document
                    .select(&content_candidate_selector)
                    .filter(|element| !is_in_excluded_context(*element))
                    .max_by_key(|element| content_score(*element, &block_selector))
            })
            .or_else(|| document.select(&body_selector).next());

        let has_article = document
            .select(&article_selector)
            .any(|article| !is_in_excluded_context(article));
        let has_semantic_main = document
            .select(&main_selector)
            .any(|main| !is_in_excluded_context(main));
        let is_index_page = root.is_some_and(|root| {
            !has_article
                && is_link_heavy_index(
                    root,
                    &anchor_selector,
                    &repeated_item_selector,
                    &paragraph_selector,
                    has_semantic_main,
                )
        });
        let byline = (!is_index_page)
            .then(|| extract_byline(&document))
            .flatten();
        let mut blocks = Vec::new();
        let mut links = Vec::new();
        if let Some(root) = root {
            if is_index_page {
                let items = extract_index_links(
                    root,
                    &anchor_selector,
                    &repeated_item_selector,
                    &title_link_selector,
                    &page_url,
                    &mut links,
                );
                if !items.is_empty() {
                    blocks.push(ReaderBlock::LinkList(items));
                }
            } else {
                for element in root.select(&block_selector) {
                    if has_block_ancestor(element) || is_in_excluded_context(element) {
                        continue;
                    }
                    if is_in_navigation_list(element, &list_item_selector, &anchor_selector) {
                        continue;
                    }
                    let tag = element.value().name();
                    let block = match tag {
                        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                            let level = tag[1..].parse::<u8>().unwrap_or(6);
                            ReaderBlock::Heading {
                                level,
                                content: extract_inline(element, &page_url, &mut links),
                            }
                        }
                        "ul" | "ol" => {
                            let items = element
                                .select(&list_item_selector)
                                .filter(|item| {
                                    item.ancestors()
                                        .filter_map(ElementRef::wrap)
                                        .find(|ancestor| {
                                            matches!(ancestor.value().name(), "ul" | "ol")
                                        })
                                        .is_some_and(|parent_list| parent_list == element)
                                })
                                .filter(|item| !is_in_excluded_context(*item))
                                .map(|item| extract_inline(item, &page_url, &mut links))
                                .filter(|item| !inline_is_empty(item))
                                .collect::<Vec<_>>();
                            if items.is_empty() {
                                continue;
                            }
                            ReaderBlock::List {
                                ordered: tag == "ol",
                                items,
                            }
                        }
                        "blockquote" => {
                            ReaderBlock::Blockquote(extract_inline(element, &page_url, &mut links))
                        }
                        "pre" => ReaderBlock::Code(raw_text(element)),
                        _ => ReaderBlock::Paragraph(extract_inline(element, &page_url, &mut links)),
                    };
                    if !block_is_empty(&block) && blocks.last() != Some(&block) {
                        blocks.push(block);
                    }
                }
                if blocks.is_empty() {
                    let content = extract_inline(root, &page_url, &mut links);
                    if !inline_is_empty(&content) {
                        blocks.push(ReaderBlock::Paragraph(content));
                    }
                }
            }
        }

        let images = root
            .into_iter()
            .flat_map(|root| root.select(&image_selector))
            .filter(|image| !is_in_excluded_context(*image))
            .filter_map(|image| {
                let source = image_source(image)?;
                let source = source.trim();
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
                    referer: page_url.to_string(),
                })
            })
            .take(100)
            .collect::<Vec<_>>();

        let forms: Vec<HtmlForm> = root
            .into_iter()
            .flat_map(|root| root.select(&form_selector))
            .filter_map(|form| parse_form(form, &page_url))
            .collect();

        let reader = ReaderDocument {
            title: title.clone(),
            byline,
            blocks,
            links: links.clone(),
        };
        let content = reader_to_text(&reader);

        Ok(Article {
            title,
            content,
            links,
            images,
            forms,
            reader,
        })
    }
}

fn has_block_ancestor(element: ElementRef<'_>) -> bool {
    element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(|ancestor| {
            matches!(
                ancestor.value().name(),
                "h1" | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "p"
                    | "li"
                    | "ul"
                    | "ol"
                    | "blockquote"
                    | "pre"
                    | "td"
                    | "th"
            )
        })
}

fn content_score(element: ElementRef<'_>, block_selector: &Selector) -> usize {
    let text_length = normalize_text(clean_text(element).iter().map(String::as_str)).len();
    let element_count = element
        .descendants()
        .filter(|node| ElementRef::wrap(*node).is_some())
        .count()
        .max(1);
    let block_count = element
        .select(block_selector)
        .filter(|block| !is_in_excluded_context(*block))
        .count();
    (text_length.saturating_mul(text_length) / element_count)
        .saturating_add(block_count.saturating_mul(20))
}

fn is_link_heavy_index(
    root: ElementRef<'_>,
    anchor_selector: &Selector,
    repeated_item_selector: &Selector,
    paragraph_selector: &Selector,
    has_semantic_main: bool,
) -> bool {
    let paragraph_text_length = root
        .select(paragraph_selector)
        .filter(|paragraph| !is_in_excluded_context(*paragraph))
        .map(|paragraph| normalize_text(paragraph.text()).chars().count())
        .sum::<usize>();
    if paragraph_text_length > 500 {
        return false;
    }

    let text_length = normalize_text(clean_text(root).iter().map(String::as_str)).len();
    if text_length == 0 {
        return false;
    }

    let anchors = root
        .select(anchor_selector)
        .filter(|anchor| !is_in_excluded_context(*anchor))
        .filter_map(|anchor| {
            let text = normalize_text(anchor.text());
            (!text.is_empty() && anchor.value().attr("href").is_some()).then_some(text)
        })
        .collect::<Vec<_>>();
    if anchors.len() < 8 {
        return false;
    }
    let linked_text_length = anchors.iter().map(String::len).sum::<usize>();
    let link_text_density = linked_text_length as f64 / text_length as f64;

    let repeated_items = root
        .select(repeated_item_selector)
        .filter(|item| !is_in_excluded_context(*item))
        .collect::<Vec<_>>();
    let linked_repeated_items = repeated_items
        .iter()
        .filter(|item| item.select(anchor_selector).next().is_some())
        .count();
    let has_repetitive_link_list = repeated_items.len() >= 5
        && linked_repeated_items.saturating_mul(5) >= repeated_items.len().saturating_mul(3);

    link_text_density >= 0.30 && (has_repetitive_link_list || !has_semantic_main)
}

fn extract_index_links(
    root: ElementRef<'_>,
    anchor_selector: &Selector,
    repeated_item_selector: &Selector,
    title_link_selector: &Selector,
    page_url: &Url,
    links: &mut Vec<String>,
) -> Vec<ReaderLinkItem> {
    let title_links = root
        .select(title_link_selector)
        .filter_map(|anchor| index_link_item(anchor, page_url, links))
        .take(100)
        .collect::<Vec<_>>();
    if !title_links.is_empty() {
        return title_links;
    }

    let repeated_links = root
        .select(repeated_item_selector)
        .filter(|item| !is_in_excluded_context(*item))
        .filter(|item| {
            item.ancestors()
                .filter_map(ElementRef::wrap)
                .find(|ancestor| matches!(ancestor.value().name(), "li" | "tr"))
                .is_none()
        })
        .filter_map(|item| {
            item.select(anchor_selector)
                .filter(|anchor| !is_in_excluded_context(*anchor))
                .filter_map(|anchor| {
                    let text = normalize_text(anchor.text());
                    (text.chars().count() >= 4).then_some((anchor, text))
                })
                .max_by_key(|(_, text)| text.chars().count())
                .and_then(|(anchor, _)| index_link_item(anchor, page_url, links))
        })
        .take(100)
        .collect::<Vec<_>>();
    if !repeated_links.is_empty() {
        return repeated_links;
    }

    root.select(anchor_selector)
        .filter(|anchor| !is_in_excluded_context(*anchor))
        .filter_map(|anchor| index_link_item(anchor, page_url, links))
        .take(100)
        .collect()
}

fn index_link_item(
    anchor: ElementRef<'_>,
    page_url: &Url,
    links: &mut Vec<String>,
) -> Option<ReaderLinkItem> {
    let text = normalize_text(anchor.text());
    if text.chars().count() < 4 {
        return None;
    }
    let url = resolve_link(anchor.value().attr("href")?, page_url)?;
    let link_index = if let Some(index) = links.iter().position(|existing| existing == &url) {
        index
    } else {
        links.push(url);
        links.len() - 1
    };
    Some(ReaderLinkItem { text, link_index })
}

fn extract_byline(document: &Html) -> Option<String> {
    let selectors = [
        "meta[name=author]",
        "meta[property='article:author']",
        "[rel=author]",
        ".byline",
        ".author",
        "[itemprop=author]",
    ];
    let mut author = None;
    for selector_text in selectors {
        let Ok(selector) = Selector::parse(selector_text) else {
            continue;
        };
        if let Some(value) = document.select(&selector).find_map(|element| {
            element
                .value()
                .attr("content")
                .map(str::to_owned)
                .or_else(|| Some(normalize_text(element.text())))
                .filter(|value| !value.is_empty())
        }) {
            author = Some(value);
            break;
        }
    }

    let date_selector = Selector::parse("time[datetime], time").ok()?;
    let date = document.select(&date_selector).find_map(|element| {
        let date = element
            .value()
            .attr("datetime")
            .map(str::to_owned)
            .unwrap_or_else(|| normalize_text(element.text()));
        (!date.is_empty()).then_some(date)
    });

    match (author, date) {
        (Some(author), Some(date)) if author != date => Some(format!("By {author}  |  {date}")),
        (Some(author), _) => Some(format!("By {author}")),
        (None, Some(date)) => Some(date),
        (None, None) => None,
    }
}

fn extract_inline(
    element: ElementRef<'_>,
    page_url: &Url,
    links: &mut Vec<String>,
) -> Vec<ReaderInline> {
    let mut output = Vec::new();
    for node in element.descendants() {
        let Some(text) = node.value().as_text() else {
            continue;
        };
        if node
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(is_excluded)
        {
            continue;
        }
        let anchor = node
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|ancestor| ancestor.value().name() == "a");
        let inline_code = node
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|ancestor| matches!(ancestor.value().name(), "code" | "kbd" | "samp"));
        if inline_code {
            append_inline_code(&mut output, &text.text);
        } else {
            let link_index = anchor
                .and_then(|anchor| anchor.value().attr("href"))
                .and_then(|href| resolve_link(href, page_url))
                .map(|url| {
                    if let Some(index) = links.iter().position(|existing| existing == &url) {
                        index
                    } else {
                        links.push(url);
                        links.len() - 1
                    }
                });
            append_inline_text_with_markup(&mut output, &text.text, link_index);
        }
    }
    output
}

fn append_inline_text_with_markup(
    output: &mut Vec<ReaderInline>,
    text: &str,
    link_index: Option<usize>,
) {
    let mut remaining = text;
    while let Some((start, end)) = next_literal_html_tag(remaining) {
        append_inline_text(output, &remaining[..start], link_index);
        append_inline_code(output, &remaining[start..end]);
        remaining = &remaining[end..];
    }
    append_inline_text(output, remaining, link_index);
}

fn next_literal_html_tag(text: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(relative_start) = text.get(search_from..)?.find('<') {
        let start = search_from + relative_start;
        let after_open = start + 1;
        let relative_end = text.get(after_open..)?.find('>')?;
        let end = after_open + relative_end + 1;
        let candidate = text.get(after_open..end - 1)?;
        if is_tag_like_text(candidate) {
            if !candidate.trim_end().ends_with('/') && !candidate.trim_start().starts_with('/') {
                if let Some(name) = literal_tag_name(candidate) {
                    if let Some(close_end) = matching_literal_close(text, end, &name) {
                        return Some((start, close_end));
                    }
                }
            }
            return Some((start, end));
        }
        search_from = end;
        if search_from >= text.len() {
            return None;
        }
    }
    None
}

fn literal_tag_name(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    let candidate = candidate
        .strip_prefix('/')
        .unwrap_or(candidate)
        .trim_start();
    let name = candidate
        .split(|character: char| character.is_whitespace() || character == '/')
        .next()?;
    Some(name.to_ascii_lowercase())
}

fn matching_literal_close(text: &str, search_from: usize, tag_name: &str) -> Option<usize> {
    let mut cursor = search_from;
    while let Some(relative_start) = text.get(cursor..)?.find('<') {
        let start = cursor + relative_start;
        let after_open = start + 1;
        let relative_end = text.get(after_open..)?.find('>')?;
        let end = after_open + relative_end + 1;
        let candidate = text.get(after_open..end - 1)?.trim();
        let Some(closing) = candidate.strip_prefix('/') else {
            cursor = end;
            continue;
        };
        if literal_tag_name(&format!("/{closing}")).as_deref() == Some(tag_name) {
            return Some(end);
        }
        cursor = end;
    }
    None
}

fn is_tag_like_text(candidate: &str) -> bool {
    let candidate = candidate.trim();
    let candidate = candidate
        .strip_prefix('/')
        .unwrap_or(candidate)
        .trim_start();
    let name = candidate
        .split(|character: char| character.is_whitespace() || character == '/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let known_tag = matches!(
        name.as_str(),
        "a" | "abbr"
            | "article"
            | "aside"
            | "b"
            | "blockquote"
            | "body"
            | "br"
            | "button"
            | "code"
            | "div"
            | "em"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "html"
            | "i"
            | "img"
            | "li"
            | "link"
            | "main"
            | "meta"
            | "ol"
            | "p"
            | "pre"
            | "script"
            | "section"
            | "small"
            | "span"
            | "strong"
            | "style"
            | "table"
            | "td"
            | "th"
            | "title"
            | "tr"
            | "u"
            | "ul"
            | "!doctype"
    );
    known_tag || name.contains('-')
}

fn append_inline_code(output: &mut Vec<ReaderInline>, code: &str) {
    let code = code.trim();
    if code.is_empty() {
        return;
    }
    let previous_char = output.iter().rev().find_map(|inline| match inline {
        ReaderInline::Text(text) | ReaderInline::Code(text) | ReaderInline::Link { text, .. } => {
            text.chars().last()
        }
    });
    let starts_with_closing_punctuation = code
        .chars()
        .next()
        .is_some_and(|character| ".,!?;:%)]}»”’".contains(character));
    let needs_separator = previous_char.is_some_and(|character| {
        !character.is_whitespace()
            && !starts_with_closing_punctuation
            && !"([{«“‘".contains(character)
    });
    if needs_separator {
        output.push(ReaderInline::Text(" ".to_owned()));
    }
    output.push(ReaderInline::Code(code.to_owned()));
}

fn resolve_link(href: &str, page_url: &Url) -> Option<String> {
    let href = href.trim();
    if href.is_empty()
        || href.to_ascii_lowercase().starts_with("javascript:")
        || href.to_ascii_lowercase().starts_with("data:")
    {
        return None;
    }
    let url = page_url.join(href).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.into())
}

fn append_inline_text(output: &mut Vec<ReaderInline>, text: &str, link_index: Option<usize>) {
    let mut pending = String::new();
    for word in text.split_whitespace() {
        let starts_with_closing_punctuation = word
            .chars()
            .next()
            .is_some_and(|character| ".,!?;:%)]}»”’".contains(character));
        let follows_opening_punctuation = pending
            .chars()
            .last()
            .or_else(|| {
                output.iter().rev().find_map(|inline| match inline {
                    ReaderInline::Text(text)
                    | ReaderInline::Code(text)
                    | ReaderInline::Link { text, .. } => text.chars().last(),
                })
            })
            .is_some_and(|character| "([{«“‘".contains(character));
        let has_content = !output.is_empty() || !pending.is_empty();
        if has_content && !starts_with_closing_punctuation && !follows_opening_punctuation {
            pending.push(' ');
        }
        pending.push_str(word);
    }
    if pending.is_empty() {
        return;
    }

    match (output.last_mut(), link_index) {
        (Some(ReaderInline::Text(existing)), None) => {
            existing.push_str(&pending);
        }
        (
            Some(ReaderInline::Link {
                text: existing,
                link_index: existing_index,
            }),
            Some(index),
        ) if *existing_index == index => {
            existing.push_str(&pending);
        }
        _ => match link_index {
            Some(link_index) => output.push(ReaderInline::Link {
                text: pending,
                link_index,
            }),
            None => output.push(ReaderInline::Text(pending)),
        },
    }
}

fn inline_is_empty(inline: &[ReaderInline]) -> bool {
    inline.iter().all(|part| match part {
        ReaderInline::Text(text) | ReaderInline::Code(text) | ReaderInline::Link { text, .. } => {
            text.trim().is_empty()
        }
    })
}

fn block_is_empty(block: &ReaderBlock) -> bool {
    match block {
        ReaderBlock::Heading { content, .. }
        | ReaderBlock::Paragraph(content)
        | ReaderBlock::Blockquote(content) => inline_is_empty(content),
        ReaderBlock::List { items, .. } => items.iter().all(|item| inline_is_empty(item)),
        ReaderBlock::LinkList(items) => items.is_empty(),
        ReaderBlock::Code(code) => code.trim().is_empty(),
    }
}

fn reader_to_text(document: &ReaderDocument) -> String {
    let mut lines = Vec::new();
    if let Some(byline) = &document.byline {
        lines.push(byline.clone());
        lines.push(String::new());
    }
    for block in &document.blocks {
        let text = match block {
            ReaderBlock::Heading { level, content } => {
                format!(
                    "{} {}",
                    "#".repeat(usize::from(*level)),
                    inline_to_text(content)
                )
            }
            ReaderBlock::Paragraph(content) => inline_to_text(content),
            ReaderBlock::Blockquote(content) => format!("> {}", inline_to_text(content)),
            ReaderBlock::Code(code) => code.clone(),
            ReaderBlock::List { ordered, items } => items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let marker = if *ordered {
                        format!("{}. ", index + 1)
                    } else {
                        "• ".to_owned()
                    };
                    format!("{marker}{}", inline_to_text(item))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            ReaderBlock::LinkList(items) => items
                .iter()
                .map(|item| format!("[{}] {}", item.link_index + 1, item.text))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        if !text.is_empty() {
            lines.push(text);
            lines.push(String::new());
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn inline_to_text(content: &[ReaderInline]) -> String {
    content
        .iter()
        .map(|part| match part {
            ReaderInline::Text(text) | ReaderInline::Code(text) => text.clone(),
            ReaderInline::Link { text, link_index } => {
                format!("{text} [Link #{}]", link_index + 1)
            }
        })
        .collect::<String>()
}

fn is_in_navigation_list(
    element: ElementRef<'_>,
    item_selector: &Selector,
    link_selector: &Selector,
) -> bool {
    let list = if matches!(element.value().name(), "ul" | "ol") {
        element
    } else if let Some(list) = element
        .ancestors()
        .filter_map(ElementRef::wrap)
        .find(|ancestor| matches!(ancestor.value().name(), "ul" | "ol"))
    {
        list
    } else {
        return false;
    };
    let items = list.select(item_selector).collect::<Vec<_>>();
    if items.is_empty() {
        return false;
    }
    let linked_items = items
        .iter()
        .filter(|item| item.select(link_selector).next().is_some())
        .count();
    linked_items.saturating_mul(3) >= items.len().saturating_mul(2)
        && items.len() >= 3
        && element
            .ancestors()
            .filter_map(ElementRef::wrap)
            .find(|ancestor| matches!(ancestor.value().name(), "ul" | "ol"))
            .is_some_and(|nearest_list| nearest_list == list)
}

fn image_source<'a>(image: ElementRef<'a>) -> Option<&'a str> {
    let value = image.value();
    value
        .attr("data-original")
        .or_else(|| value.attr("data-lazy-src"))
        .or_else(|| value.attr("data-srcset").and_then(largest_srcset_url))
        .or_else(|| value.attr("srcset").and_then(largest_srcset_url))
        .or_else(|| value.attr("data-src"))
        .or_else(|| value.attr("src"))
}

fn largest_srcset_url(srcset: &str) -> Option<&str> {
    srcset
        .split(',')
        .filter_map(|candidate| {
            let mut fields = candidate.split_whitespace();
            let url = fields.next()?;
            if url.to_ascii_lowercase().starts_with("data:") {
                return None;
            }
            let descriptor = fields
                .next()
                .and_then(|value| value.strip_suffix('w').or_else(|| value.strip_suffix('x')))
                .and_then(|value| value.parse::<f32>().ok())
                .unwrap_or(1.0);
            Some((url, descriptor))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(url, _)| url)
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
    let mut output = String::new();
    for word in parts.flat_map(str::split_whitespace) {
        let starts_with_closing_punctuation = word
            .chars()
            .next()
            .is_some_and(|character| ".,!?;:%)]}»”’".contains(character));
        let follows_opening_punctuation = output
            .chars()
            .last()
            .is_some_and(|character| "([{«“‘".contains(character));
        if !output.is_empty() && !starts_with_closing_punctuation && !follows_opening_punctuation {
            output.push(' ');
        }
        output.push_str(word);
    }
    output
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

fn raw_text(element: ElementRef<'_>) -> String {
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
    let tag_is_excluded = matches!(
        element.value().name(),
        "script"
            | "style"
            | "noscript"
            | "iframe"
            | "svg"
            | "canvas"
            | "template"
            | "nav"
            | "header"
            | "aside"
            | "footer"
            | "form"
    );
    let class_or_id = format!(
        "{} {}",
        element.value().attr("class").unwrap_or_default(),
        element.value().attr("id").unwrap_or_default()
    )
    .to_ascii_lowercase();
    let looks_like_boilerplate = [
        "navigation",
        "navbar",
        "sidebar",
        "advert",
        "cookie",
        "related",
        "recommend",
        "social-share",
    ]
    .iter()
    .any(|token| class_or_id.contains(token));
    tag_is_excluded
        || looks_like_boilerplate
        || element.attr("hidden").is_some()
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
    use super::{FormFieldKind, FormMethod, HtmlParser, ReaderBlock, ReaderInline, ReaderLinkItem};

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
        assert!(article.content.contains("Readable article [Link #1] text."));
        assert!(!article.content.contains("tracking"));
        assert!(!article.content.contains("Navigation noise"));
        assert!(!article.content.contains("Hidden text"));
        assert_eq!(article.links, ["https://example.test/page"]);
        assert!(matches!(
            &article.reader.blocks[1],
            ReaderBlock::Paragraph(parts)
                if parts.contains(&ReaderInline::Link {
                    text: " article".to_owned(),
                    link_index: 0,
                })
        ));
    }

    #[test]
    fn keeps_article_blocks_in_reading_order_without_nested_duplicates() {
        let article = match HtmlParser::parse_article_at(
            r#"
                <html><body>
                  <nav><a href="/menu">Navigation</a></nav>
                  <main>
                    <h1>Chapter title</h1>
                    <p>First paragraph with <a href="/one">a link</a>.</p>
                    <ul><li>First item</li><li>Second item</li></ul>
                    <section><p>Last paragraph.</p></section>
                  </main>
                </body></html>
            "#,
            "https://comics.example/chapter",
        ) {
            Ok(article) => article,
            Err(error) => panic!("semantic article parsing failed: {error}"),
        };

        assert!(
            article.content.contains("# Chapter title"),
            "unexpected reader output: {:?}",
            article.content
        );
        assert!(article
            .content
            .contains("First paragraph with a link [Link #1]."));
        assert!(article.content.contains("• First item\n• Second item"));
        assert!(article.content.contains("Last paragraph."));
        assert!(!article.content.contains("Links\n-----"));
        assert!(!article.content.contains("Navigation"));
        assert!(matches!(
            article.reader.blocks.as_slice(),
            [
                ReaderBlock::Heading { level: 1, .. },
                ReaderBlock::Paragraph(_),
                ReaderBlock::List {
                    ordered: false,
                    items,
                },
                ReaderBlock::Paragraph(_),
            ] if items.len() == 2
        ));
    }

    #[test]
    fn extracts_byline_and_inline_links_while_dropping_page_boilerplate() {
        let article = match HtmlParser::parse_article_at(
            r#"
                <html>
                  <head>
                    <meta name="author" content="A. Writer">
                    <title>Story page</title>
                  </head>
                  <body>
                    <header><p>Header noise</p></header>
                    <nav><ul><li><a href="/home">Home</a></li></ul></nav>
                    <main>
                      <article>
                        <h1>A useful story</h1>
                        <time datetime="2026-11-05">November 5</time>
                        <p>Read <a href="/story">the full story</a>.</p>
                        <ul class="related-links">
                          <li><a href="/related/1">Related one</a></li>
                          <li><a href="/related/2">Related two</a></li>
                          <li><a href="/related/3">Related three</a></li>
                        </ul>
                        <blockquote>A thoughtful quotation.</blockquote>
                        <ol><li>First step</li><li>Second step</li></ol>
                      </article>
                    </main>
                    <footer><p>Footer noise</p></footer>
                  </body>
                </html>
            "#,
            "https://news.example/article",
        ) {
            Ok(article) => article,
            Err(error) => panic!("semantic reader extraction failed: {error}"),
        };

        assert_eq!(article.reader.title, "Story page");
        assert_eq!(
            article.reader.byline.as_deref(),
            Some("By A. Writer  |  2026-11-05")
        );
        assert_eq!(article.links, ["https://news.example/story"]);
        assert!(article.content.contains("Read the full story [Link #1]."));
        assert!(!article.content.contains("Header noise"));
        assert!(!article.content.contains("Footer noise"));
        assert!(!article.content.contains("Related one"));
        assert!(matches!(
            article.reader.blocks.as_slice(),
            [
                ReaderBlock::Heading { level: 1, .. },
                ReaderBlock::Paragraph(_),
                ReaderBlock::Blockquote(_),
                ReaderBlock::List {
                    ordered: true,
                    items,
                },
            ] if items.len() == 2
        ));
    }

    #[test]
    fn keeps_escaped_html_examples_as_inline_code_without_duplicate_text() {
        let article = match HtmlParser::parse_article_at(
            r#"
                <html><body><article>
                  <h1>HTML guide</h1>
                  <p>The markup <code>&lt;p&gt;</code> starts a paragraph. The literal
                     example &lt;a href="/page"&gt;link&lt;/a&gt; remains documentation.</p>
                </article></body></html>
            "#,
            "https://docs.example/html",
        ) {
            Ok(article) => article,
            Err(error) => panic!("escaped HTML example parsing failed: {error}"),
        };
        let paragraph = article
            .reader
            .blocks
            .iter()
            .find_map(|block| match block {
                ReaderBlock::Paragraph(parts) => Some(parts),
                _ => None,
            })
            .expect("paragraph should be extracted once");

        assert!(paragraph.contains(&ReaderInline::Code("<p>".to_owned())));
        assert!(paragraph.contains(&ReaderInline::Code("<a href=\"/page\">link</a>".to_owned())));
        assert_eq!(article.reader.blocks.len(), 2);
        assert_eq!(article.content.matches("The markup").count(), 1);
        assert_eq!(article.content.matches("literal").count(), 1);
        assert!(article.content.contains("<a href=\"/page\">"));
        assert!(article.links.is_empty());
    }

    #[test]
    fn nested_list_paragraphs_are_extracted_only_as_list_items() {
        let article = match HtmlParser::parse_article_at(
            "<main><ul><li><p>One nested paragraph.</p></li><li><p>Another nested paragraph.</p></li></ul></main>",
            "https://example.test/",
        ) {
            Ok(article) => article,
            Err(error) => panic!("nested list parsing failed: {error}"),
        };

        assert_eq!(article.content.matches("One nested paragraph").count(), 1);
        assert_eq!(
            article.content.matches("Another nested paragraph").count(),
            1
        );
        assert!(matches!(
            article.reader.blocks.as_slice(),
            [ReaderBlock::List { items, .. }] if items.len() == 2
        ));
    }

    #[test]
    fn preserves_whitespace_in_preformatted_code_blocks() {
        let article = match HtmlParser::parse_article_at(
            "<article><pre>&lt;p&gt;\n  &lt;a href=&quot;/x&quot;&gt;link&lt;/a&gt;\n&lt;/p&gt;</pre></article>",
            "https://docs.example/",
        ) {
            Ok(article) => article,
            Err(error) => panic!("preformatted code parsing failed: {error}"),
        };

        assert_eq!(
            article.reader.blocks,
            [ReaderBlock::Code(
                "<p>\n  <a href=\"/x\">link</a>\n</p>".to_owned()
            )]
        );
        assert!(article.links.is_empty());
    }

    #[test]
    fn renders_link_dense_aggregator_as_a_clean_link_list() {
        let rows = (1..=10)
            .map(|index| {
                format!(
                    r#"<tr><td class="titleline"><a href="/story/{index}">Interesting story number {index}</a></td></tr>"#
                )
            })
            .collect::<String>();
        let html = format!(
            "<html><head><title>News index</title></head><body><table>{rows}</table></body></html>"
        );
        let article = match HtmlParser::parse_article_at(&html, "https://news.example/") {
            Ok(article) => article,
            Err(error) => panic!("aggregator parsing failed: {error}"),
        };

        assert_eq!(article.links.len(), 10);
        assert!(article.content.contains("[1] Interesting story number 1"));
        assert!(!article.content.contains("[Link #"));
        assert!(matches!(
            article.reader.blocks.as_slice(),
            [ReaderBlock::LinkList(items)] if items.len() == 10
        ));
        assert_eq!(
            article.reader.blocks[0],
            ReaderBlock::LinkList(
                (1..=10)
                    .map(|index| ReaderLinkItem {
                        text: format!("Interesting story number {index}"),
                        link_index: index - 1,
                    })
                    .collect()
            )
        );
    }

    #[test]
    fn keeps_article_links_inline_even_with_repetitive_related_links() {
        let related = (1..=10)
            .map(|index| {
                format!(r#"<li><a href="/related/{index}">Related story {index}</a></li>"#)
            })
            .collect::<String>();
        let html = format!(
            "<html><body><article><h1>Feature story</h1><p>Main article text.</p><ul>{related}</ul></article></body></html>"
        );
        let article = match HtmlParser::parse_article_at(&html, "https://news.example/story") {
            Ok(article) => article,
            Err(error) => panic!("article parsing failed: {error}"),
        };

        assert!(article.content.contains("Main article text."));
        assert!(!matches!(
            article.reader.blocks.first(),
            Some(ReaderBlock::LinkList(_))
        ));
    }

    #[test]
    fn substantial_link_rich_article_paragraphs_never_fall_back_to_index_list() {
        let linked_prose = (1..=12)
            .map(|index| {
                format!(
                    r#"<a href="/reference/{index}">linked reference text with context {index} and details</a>"#
                )
            })
            .collect::<Vec<_>>()
            .join(" provides context. ");
        let paragraph = format!(
            "This encyclopedia article explains a complex subject in detail. {linked_prose} {}",
            "Additional explanatory prose gives readers context and connects the references to the surrounding discussion. ".repeat(3)
        );
        assert!(paragraph.chars().count() > 500);
        let html = format!(
            "<html><body><main><article><h1>Encyclopedia entry</h1><p>{paragraph}</p></article></main></body></html>"
        );
        let article = match HtmlParser::parse_article_at(&html, "https://wiki.example/page") {
            Ok(article) => article,
            Err(error) => panic!("link-rich article parsing failed: {error}"),
        };

        assert_eq!(article.links.len(), 12);
        assert!(article.content.contains("[Link #1]"));
        assert!(!matches!(
            article.reader.blocks.first(),
            Some(ReaderBlock::LinkList(_))
        ));
    }

    #[test]
    fn detects_unstructured_link_dense_indexes_from_link_text_density() {
        let links = (1..=9)
            .map(|index| {
                format!(r#"<a href="/item/{index}">A highly relevant item title {index}</a>"#)
            })
            .collect::<String>();
        let html = format!(
            "<html><head><title>Directory</title></head><body><div>{links}</div></body></html>"
        );
        let article = match HtmlParser::parse_article_at(&html, "https://directory.example/") {
            Ok(article) => article,
            Err(error) => panic!("link density fallback failed: {error}"),
        };

        assert_eq!(article.links.len(), 9);
        assert!(matches!(
            article.reader.blocks.as_slice(),
            [ReaderBlock::LinkList(items)] if items.len() == 9
        ));
        assert!(!article.content.contains("[Link #"));
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
        assert!(!article.content.contains("[FORM 1] Sign in"));
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
        assert_eq!(
            article.images[0].referer,
            "https://comics.example/series/chapter-1"
        );
        assert!(!article.content.contains("[IMAGE 1]"));
        assert_eq!(article.reader.blocks.len(), 1);
    }

    #[test]
    fn prefers_full_resolution_comic_images_from_common_lazy_attributes() {
        let article = match HtmlParser::parse_article_at(
            r#"
                <html><body>
                  <img src="/thumb.jpg" data-src="/lazy.jpg"
                       srcset="/small.jpg 320w, /large.jpg 1280w"
                       alt="Panel">
                  <img data-original="/original.webp" src="/placeholder.gif">
                </body></html>
            "#,
            "https://comics.example/chapter/1",
        ) {
            Ok(article) => article,
            Err(error) => panic!("comic images failed to parse: {error}"),
        };
        assert_eq!(article.images.len(), 2);
        assert_eq!(article.images[0].url, "https://comics.example/large.jpg");
        assert_eq!(
            article.images[1].url,
            "https://comics.example/original.webp"
        );
    }
}
