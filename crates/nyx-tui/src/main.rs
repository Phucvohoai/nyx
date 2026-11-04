use std::{io, time::Duration};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::{ImageReader, RgbaImage};
use nyx_core::{
    default_searxng_endpoint, normalize_direct_url, parse_searxng_results, searxng_search_url,
    AppState, AppView, Article, ArticleImage, FormFieldKind, FormMethod, HtmlForm, HtmlParser,
    ReaderBlock, ReaderDocument, ReaderInline, ReaderLinkItem, SearchResult, RESULTS_PER_PAGE,
};
use nyx_pal::{
    FormRequestMethod, FormSubmission, NetworkAdapter, StdBackend, SystemAdapter, SystemMetrics,
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Terminal,
};
use std::io::Cursor;
use tokio::sync::mpsc;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

const DEFAULT_TOR_PROXY: &str = "socks5h://127.0.0.1:9050";

struct SearchCompletion {
    query: String,
    result: Result<Vec<SearchResult>, String>,
}

struct PageCompletion {
    navigation_id: u64,
    result: Result<(Article, Option<String>), String>,
}

fn is_current_navigation(completion_id: u64, current_id: u64) -> bool {
    completion_id == current_id
}

struct ImageViewer {
    index: usize,
    image: RgbaImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinkHitbox {
    link_index: usize,
    row: u16,
    column_start: u16,
    column_end: u16,
}

enum ActiveModal<'a> {
    Form(&'a FormSession),
    Image(&'a ImageViewer),
}

struct UiContext<'a> {
    selected_link: usize,
    show_help: bool,
    notice: Option<&'a str>,
    modal: Option<ActiveModal<'a>>,
    tor_configured: bool,
    tor_connected: bool,
}

#[derive(Debug)]
struct FormSession {
    form: HtmlForm,
    values: Vec<String>,
    active_field: usize,
}

impl Drop for FormSession {
    fn drop(&mut self) {
        self.values.iter_mut().for_each(Zeroize::zeroize);
    }
}

impl FormSession {
    fn new(form: HtmlForm) -> Self {
        let values = form
            .fields
            .iter()
            .map(|field| field.value.clone())
            .collect();
        let active_field = form
            .fields
            .iter()
            .position(|field| field.kind != FormFieldKind::Hidden)
            .unwrap_or(0);
        Self {
            form,
            values,
            active_field,
        }
    }

    fn submission(&self) -> FormSubmission {
        FormSubmission {
            url: self.form.action.clone(),
            method: match self.form.method {
                FormMethod::Get => FormRequestMethod::Get,
                FormMethod::Post => FormRequestMethod::Post,
            },
            fields: self
                .form
                .fields
                .iter()
                .zip(&self.values)
                .filter(|(field, value)| {
                    !matches!(field.kind, FormFieldKind::Checkbox | FormFieldKind::Radio)
                        || !value.is_empty()
                })
                .map(|(field, value)| (field.name.clone(), value.clone()))
                .collect(),
        }
    }
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

struct TerminalSession;

impl TerminalSession {
    fn enter() -> io::Result<(Self, Tui)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok((Self, terminal)),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(
                    stdout,
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tor_proxy = std::env::var_os("NYX_TOR_SOCKS")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| "NYX_TOR_SOCKS must be valid Unicode")
        })
        .transpose()?;
    let tor_proxy_url = tor_proxy
        .clone()
        .unwrap_or_else(|| DEFAULT_TOR_PROXY.to_owned());
    let vault_path = StdBackend::default_session_vault_path()?;
    let vault_exists = vault_path.exists();
    let prompt = if vault_exists {
        "Unlock Nyx session vault: "
    } else {
        "Create a passphrase for the Nyx session vault: "
    };
    let passphrase = Zeroizing::new(rpassword::prompt_password(prompt)?);
    if passphrase.is_empty() {
        return Err("session vault passphrase cannot be empty".into());
    }
    if !vault_exists {
        let confirmation =
            Zeroizing::new(rpassword::prompt_password("Confirm vault passphrase: ")?);
        if passphrase != confirmation {
            return Err("session vault passphrases do not match".into());
        }
    }
    let (mut backend, _) = match tor_proxy.as_deref() {
        Some(proxy) => {
            StdBackend::open_session_vault_with_tor_proxy(&vault_path, passphrase.as_str(), proxy)?
        }
        None => StdBackend::open_session_vault(&vault_path, passphrase.as_str())?,
    };
    let tor_startup_notice = if backend.tor_proxy_configured() {
        match backend.verify_tor_route().await {
            Ok(()) => None,
            Err(error) => Some(format!(
                "Tor verification failed; network requests are blocked: {error}"
            )),
        }
    } else {
        None
    };

    let (_session, mut terminal) = TerminalSession::enter()?;
    let mut state = AppState::default();
    let mut selected_link = 0usize;
    let mut show_help = false;
    let mut bookmark_focus = false;
    let mut notice = tor_startup_notice;
    let mut active_form = None;
    let mut image_viewer: Option<ImageViewer> = None;
    let mut next_form_index = 0usize;
    let mut active_searches = 0usize;
    let mut active_navigation = false;
    let mut navigation_id = 0u64;
    let mut active_navigation_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut link_hitboxes = Vec::new();
    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<SearchCompletion>();
    let (page_tx, mut page_rx) = mpsc::unbounded_channel::<PageCompletion>();

    loop {
        while let Ok(completion) = search_rx.try_recv() {
            active_searches = active_searches.saturating_sub(1);
            if matches!(
                &state.view,
                AppView::SearchResults { query, .. } if query == &completion.query
            ) {
                match completion.result {
                    Ok(results) => {
                        state.update_search_results(&completion.query, results);
                        notice = None;
                    }
                    Err(error) => notice = Some(format!("Search failed: {error}")),
                }
            }
        }
        while let Ok(completion) = page_rx.try_recv() {
            if !is_current_navigation(completion.navigation_id, navigation_id) {
                continue;
            }
            active_navigation = false;
            active_navigation_task = None;
            match completion.result {
                Ok((article, warning)) => {
                    state.open_article(
                        article.title,
                        article.content,
                        article.links,
                        article.images,
                        article.forms,
                        article.reader,
                    );
                    state.input.clear();
                    state.bookmark_input.clear();
                    selected_link = 0;
                    notice = warning.or_else(|| Some("Page loaded.".to_owned()));
                }
                Err(error) => notice = Some(format!("Page failed: {error}")),
            }
        }

        let metrics = backend.metrics()?;
        terminal.draw(|frame| {
            let context = UiContext {
                selected_link,
                show_help,
                notice: notice.as_deref(),
                modal: active_form
                    .as_ref()
                    .map(ActiveModal::Form)
                    .or_else(|| image_viewer.as_ref().map(ActiveModal::Image)),
                tor_configured: backend.tor_proxy_configured(),
                tor_connected: backend.tor_route_connected(),
            };
            render(frame, &state, &metrics, context, &mut link_hitboxes)
        })?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break;
                    } else if key.code == KeyCode::Char('l')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        navigation_id = navigation_id.wrapping_add(1);
                        active_navigation = false;
                        notice = None;
                        if let Some(task) = active_navigation_task.take() {
                            task.abort();
                        }
                        state.go_home();
                        selected_link = 0;
                        bookmark_focus = false;
                        active_form = None;
                        image_viewer = None;
                        continue;
                    } else if key.code == KeyCode::Char('t')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if active_searches > 0 {
                            notice = Some(
                                "Wait for the current search to finish before changing Tor mode."
                                    .to_owned(),
                            );
                        } else {
                            let enable_tor = !backend.tor_proxy_configured();
                            match backend.save_session() {
                                Ok(()) => {
                                    let next_backend = if enable_tor {
                                        StdBackend::open_session_vault_with_tor_proxy(
                                            &vault_path,
                                            passphrase.as_str(),
                                            &tor_proxy_url,
                                        )
                                    } else {
                                        StdBackend::open_session_vault(
                                            &vault_path,
                                            passphrase.as_str(),
                                        )
                                    };
                                    match next_backend {
                                        Ok((next_backend, _)) => {
                                            backend = next_backend;
                                            if enable_tor {
                                                notice = match backend.verify_tor_route().await {
                                                    Ok(()) => Some(
                                                        "Tor connected and verified.".to_owned(),
                                                    ),
                                                    Err(error) => Some(format!(
                                                        "Tor verification failed; network requests are blocked: {error}"
                                                    )),
                                                };
                                            } else {
                                                notice = Some(
                                                    "Tor disabled; using direct networking."
                                                        .to_owned(),
                                                );
                                            }
                                        }
                                        Err(error) => {
                                            notice = Some(format!(
                                                "Could not change network mode: {error}"
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    notice = Some(format!("Could not save session: {error}"));
                                }
                            }
                        }
                    }
                    let code = if key.code == KeyCode::Tab
                        && key.modifiers.contains(KeyModifiers::SHIFT)
                    {
                        KeyCode::BackTab
                    } else {
                        key.code
                    };
                    if image_viewer.is_some() {
                        match code {
                            KeyCode::Esc => image_viewer = None,
                            KeyCode::Left | KeyCode::Right => {
                                let images: &[ArticleImage] = match &state.view {
                                    AppView::ArticleReader { images, .. } => images,
                                    _ => &[],
                                };
                                if let Some(current) = image_viewer.as_ref() {
                                    if let Some(next) = cycle_link(
                                        current.index,
                                        images.len(),
                                        code == KeyCode::Right,
                                    ) {
                                        match load_available_article_image(
                                            &backend,
                                            images,
                                            next,
                                            code == KeyCode::Right,
                                        )
                                        .await
                                        {
                                            Ok((viewer, skipped)) => {
                                                image_viewer = Some(viewer);
                                                notice = skipped_image_notice(skipped);
                                            }
                                            Err(error) => notice = Some(error),
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else if let Some(form_session) = active_form.as_mut() {
                        if code == KeyCode::Esc {
                            active_form = None;
                            notice = None;
                        } else if code == KeyCode::Tab || code == KeyCode::BackTab {
                            cycle_form_field(form_session, code == KeyCode::Tab);
                        } else if code == KeyCode::Enter {
                            let submit_now = key.modifiers.contains(KeyModifiers::CONTROL)
                                || next_form_field(form_session, true).is_none();
                            if submit_now {
                                match validate_form(form_session) {
                                    Ok(()) => {
                                        let submission = form_session.submission();
                                        match backend.submit_form(&submission).await {
                                            Ok(page) => {
                                                let cookie_warning = page.warning.clone();
                                                match HtmlParser::parse_article_at(
                                                    &page.body,
                                                    page.url.as_str(),
                                                ) {
                                                    Ok(article) => {
                                                        state.open_article(
                                                            article.title,
                                                            article.content,
                                                            article.links,
                                                            article.images,
                                                            article.forms,
                                                            article.reader,
                                                        );
                                                        active_form = None;
                                                        selected_link = 0;
                                                        notice = cookie_warning.or_else(|| {
                                                            Some("Form submitted.".to_owned())
                                                        });
                                                    }
                                                    Err(error) => {
                                                        notice = Some(error.to_string());
                                                        active_form = None;
                                                    }
                                                }
                                            }
                                            Err(error) => {
                                                notice = Some(format!("Form failed: {error}"));
                                            }
                                        }
                                    }
                                    Err(error) => notice = Some(error),
                                }
                            } else if let Some(next) = next_form_field(form_session, true) {
                                form_session.active_field = next;
                                notice = None;
                            }
                        } else {
                            update_form_input(form_session, code);
                        }
                    } else if code == KeyCode::Char('i')
                        && matches!(state.view, AppView::ArticleReader { .. })
                    {
                        let images: &[ArticleImage] = match &state.view {
                            AppView::ArticleReader { images, .. } => images,
                            _ => &[],
                        };
                        if images.is_empty() {
                            notice = Some("This page has no supported images.".to_owned());
                        } else {
                            match load_available_article_image(&backend, images, 0, true).await {
                                Ok((viewer, skipped)) => {
                                    image_viewer = Some(viewer);
                                    notice = skipped_image_notice(skipped);
                                }
                                Err(error) => notice = Some(error),
                            }
                        }
                    } else if code == KeyCode::Char('f') {
                        if let AppView::ArticleReader { forms, .. } = &state.view {
                            if !forms.is_empty() {
                                let index = next_form_index % forms.len();
                                active_form = Some(FormSession::new(forms[index].clone()));
                                next_form_index = (index + 1) % forms.len();
                                notice = None;
                            } else {
                                notice = Some("This page has no supported HTML forms.".to_owned());
                            }
                        } else if handle_key(
                            code,
                            &mut state,
                            &mut selected_link,
                            &mut show_help,
                            &mut bookmark_focus,
                        ) {
                            break;
                        }
                    } else if code == KeyCode::Enter {
                        let target = match &state.view {
                            AppView::Home => {
                                let value = if bookmark_focus {
                                    state.bookmark_input.trim()
                                } else {
                                    state.input.trim()
                                };
                                if bookmark_focus {
                                    (!value.is_empty()).then(|| value.to_owned())
                                } else if let Some(url) = normalize_direct_url(value) {
                                    Some(url)
                                } else {
                                    state.submit_search();
                                    if let AppView::SearchResults { query, .. } = &state.view {
                                        let youtube_query =
                                            youtube_search_query(query).map(str::to_owned);
                                        notice = Some(if youtube_query.is_some() {
                                            "Searching YouTube videos...".to_owned()
                                        } else {
                                            "Searching local SearXNG...".to_owned()
                                        });
                                        let query = query.clone();
                                        let backend = backend.clone();
                                        let search_tx = search_tx.clone();
                                        active_searches = active_searches.saturating_add(1);
                                        tokio::spawn(async move {
                                            let result = if let Some(video_query) = youtube_query {
                                                if backend.tor_proxy_configured() {
                                                    Err("YouTube search is disabled in Tor mode because yt-dlp cannot currently be routed through Nyx's SOCKS proxy.".to_owned())
                                                } else {
                                                    search_youtube_videos(&video_query).await
                                                }
                                            } else {
                                                search_searxng(&backend, &query).await
                                            };
                                            let _ =
                                                search_tx.send(SearchCompletion { query, result });
                                        });
                                    }
                                    None
                                }
                            }
                            AppView::SearchResults { links, .. } => {
                                links.get(selected_link).map(|result| result.url.clone())
                            }
                            AppView::ArticleReader { links, .. } => {
                                links.get(selected_link).cloned()
                            }
                        };
                        if let Some(url) = target {
                            if active_navigation {
                                notice =
                                    Some("Wait for the current page load to finish.".to_owned());
                            } else {
                                active_navigation = true;
                                navigation_id = navigation_id.wrapping_add(1);
                                let current_navigation_id = navigation_id;
                                notice = Some(format!("Loading {url}"));
                                active_navigation_task = Some(spawn_page_load(
                                    url,
                                    backend.clone(),
                                    page_tx.clone(),
                                    current_navigation_id,
                                ));
                            }
                        }
                    } else if handle_key(
                        code,
                        &mut state,
                        &mut selected_link,
                        &mut show_help,
                        &mut bookmark_focus,
                    ) {
                        break;
                    }
                }
                Event::Paste(pasted) if matches!(state.view, AppView::Home) => {
                    if bookmark_focus {
                        append_pasted_text(&mut state.bookmark_input, &pasted);
                    } else {
                        append_pasted_text(&mut state.input, &pasted);
                    }
                    notice = None;
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.scroll = state.scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll = state.scroll.saturating_add(3);
                    }
                    MouseEventKind::Down(MouseButton::Left) => match &state.view {
                        AppView::SearchResults { links, .. } => {
                            let local_row = usize::from(mouse.row.saturating_sub(4));
                            let index = state
                                .result_page
                                .saturating_mul(RESULTS_PER_PAGE)
                                .saturating_add(local_row);
                            if let Some(result) = links.get(index) {
                                selected_link = index;
                                if active_navigation {
                                    notice = Some(
                                        "Wait for the current page load to finish.".to_owned(),
                                    );
                                } else {
                                    active_navigation = true;
                                    navigation_id = navigation_id.wrapping_add(1);
                                    notice = Some(format!("Loading {}", result.url));
                                    active_navigation_task = Some(spawn_page_load(
                                        result.url.clone(),
                                        backend.clone(),
                                        page_tx.clone(),
                                        navigation_id,
                                    ));
                                }
                            }
                        }
                        AppView::ArticleReader { links, .. } => {
                            if active_form.is_none() && image_viewer.is_none() {
                                if let Some(link_index) =
                                    hit_test_link(&link_hitboxes, mouse.column, mouse.row)
                                {
                                    if let Some(url) = links.get(link_index) {
                                        selected_link = link_index;
                                        if active_navigation {
                                            notice = Some(
                                                "Wait for the current page load to finish."
                                                    .to_owned(),
                                            );
                                        } else {
                                            active_navigation = true;
                                            navigation_id = navigation_id.wrapping_add(1);
                                            notice = Some(format!("Loading {url}"));
                                            active_navigation_task = Some(spawn_page_load(
                                                url.clone(),
                                                backend.clone(),
                                                page_tx.clone(),
                                                navigation_id,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        AppView::Home => {}
                    },
                    _ => {}
                },
                _ => {}
            }
        }
    }

    backend.save_session()?;
    drop(terminal);
    drop(_session);
    println!("Nyx session vault: {}", vault_path.display());
    Ok(())
}

fn spawn_page_load(
    url: String,
    backend: StdBackend,
    page_tx: mpsc::UnboundedSender<PageCompletion>,
    navigation_id: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let page = backend
                .fetch_page(&url)
                .await
                .map_err(|error| error.to_string())?;
            let article = HtmlParser::parse_article_at(&page.body, page.url.as_str())
                .map_err(|error| error.to_string())?;
            Ok((article, page.warning))
        }
        .await;
        let _ = page_tx.send(PageCompletion {
            navigation_id,
            result,
        });
    })
}

fn is_youtube_video_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }

    let Some(host) = url.host_str() else {
        return false;
    };
    let path_segments = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if host.eq_ignore_ascii_case("youtu.be") {
        return !path_segments.is_empty();
    }
    if !(host.eq_ignore_ascii_case("youtube.com")
        || host
            .to_ascii_lowercase()
            .strip_suffix("youtube.com")
            .is_some_and(|prefix| prefix.ends_with('.')))
    {
        return false;
    }

    if url.path() == "/watch" {
        return url
            .query_pairs()
            .any(|(key, value)| key == "v" && !value.is_empty());
    }
    matches!(
        path_segments.as_slice(),
        [kind, id, ..] if matches!(*kind, "shorts" | "embed" | "live") && !id.is_empty()
    )
}

fn append_pasted_text(input: &mut String, pasted: &str) {
    input.extend(pasted.chars().filter(|character| !character.is_control()));
}

fn next_form_field(session: &FormSession, forward: bool) -> Option<usize> {
    let field_count = session.form.fields.len();
    if field_count == 0 {
        return None;
    }

    (1..field_count)
        .map(|offset| {
            if forward {
                (session.active_field + offset) % field_count
            } else {
                (session.active_field + field_count - (offset % field_count)) % field_count
            }
        })
        .find(|index| session.form.fields[*index].kind != FormFieldKind::Hidden)
}

fn cycle_form_field(session: &mut FormSession, forward: bool) {
    if let Some(next) = next_form_field(session, forward) {
        session.active_field = next;
    }
}

fn update_form_input(session: &mut FormSession, code: KeyCode) {
    let Some(field) = session.form.fields.get(session.active_field) else {
        return;
    };
    if field.kind == FormFieldKind::Hidden {
        return;
    }
    if matches!(
        field.kind,
        FormFieldKind::Choice | FormFieldKind::Checkbox | FormFieldKind::Radio
    ) {
        let count = field.choices.len();
        if count == 0 {
            return;
        }
        let selected = field
            .choices
            .iter()
            .position(|(value, _)| value == &session.values[session.active_field])
            .unwrap_or(0);
        let next = match code {
            KeyCode::Left | KeyCode::Up => (selected + count - 1) % count,
            KeyCode::Right | KeyCode::Down => (selected + 1) % count,
            _ => return,
        };
        session.values[session.active_field] = field.choices[next].0.clone();
        return;
    }

    match code {
        KeyCode::Backspace => {
            session.values[session.active_field].pop();
        }
        KeyCode::Char(character) => session.values[session.active_field].push(character),
        _ => {}
    }
}

fn validate_form(session: &FormSession) -> Result<(), String> {
    for (field, value) in session.form.fields.iter().zip(&session.values) {
        if field.required && value.trim().is_empty() {
            return Err(format!("Required field \"{}\" is empty.", field.label));
        }
    }
    let has_password = session
        .form
        .fields
        .iter()
        .any(|field| field.kind == FormFieldKind::Password);
    if has_password
        && (session.form.method == FormMethod::Get
            || !session
                .form
                .action
                .to_ascii_lowercase()
                .starts_with("https://"))
    {
        return Err("Refusing to send a password except in an HTTPS POST form.".to_owned());
    }
    Ok(())
}

async fn load_article_image(
    backend: &dyn NetworkAdapter,
    images: &[ArticleImage],
    index: usize,
) -> Result<ImageViewer, String> {
    const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
    const MAX_IMAGE_PIXELS: u64 = 40_000_000;

    let image = images
        .get(index)
        .ok_or_else(|| "Image selection is out of range.".to_owned())?;
    let bytes = backend
        .fetch_bytes_with_referer(&image.url, &image.referer)
        .await
        .map_err(|error| format!("Image download failed: {error}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image download exceeded the 20 MiB limit.".to_owned());
    }
    let image = decode_image(&image.url, &bytes, MAX_IMAGE_PIXELS)?;
    Ok(ImageViewer { index, image })
}

async fn load_available_article_image(
    backend: &dyn NetworkAdapter,
    images: &[ArticleImage],
    start_index: usize,
    forward: bool,
) -> Result<(ImageViewer, usize), String> {
    let indices = image_indices(start_index, images.len(), forward);
    if indices.is_empty() {
        return Err("This page has no supported images.".to_owned());
    }

    let mut last_error = String::new();
    for (skipped, index) in indices.into_iter().enumerate() {
        match load_article_image(backend, images, index).await {
            Ok(viewer) => return Ok((viewer, skipped)),
            Err(error) => last_error = error,
        }
    }
    Err(format!(
        "No readable images found among {} image(s). Last error: {last_error}",
        images.len()
    ))
}

fn image_indices(start_index: usize, image_count: usize, forward: bool) -> Vec<usize> {
    if image_count == 0 || start_index >= image_count {
        return Vec::new();
    }
    (0..image_count)
        .map(|offset| {
            if forward {
                (start_index + offset) % image_count
            } else {
                (start_index + image_count - (offset % image_count)) % image_count
            }
        })
        .collect()
}

fn skipped_image_notice(skipped: usize) -> Option<String> {
    (skipped > 0).then(|| {
        format!("Skipped {skipped} unavailable image(s); showing the next readable panel.")
    })
}

fn decode_image(url: &str, bytes: &[u8], max_pixels: u64) -> Result<RgbaImage, String> {
    if looks_like_svg(bytes) {
        return decode_svg(bytes, max_pixels);
    }
    if !has_supported_image_signature(bytes) {
        return Err(unrecognized_image_response(url, bytes));
    }

    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("Could not inspect image data: {error}"))?;
    if reader.format().is_none() {
        return Err(unrecognized_image_response(url, bytes));
    }

    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("Image has invalid dimensions.".to_owned());
    }
    if u64::from(width).saturating_mul(u64::from(height)) > max_pixels {
        return Err("Image is too large to display safely.".to_owned());
    }
    image::load_from_memory(bytes)
        .map(|image| image.to_rgba8())
        .map_err(|error| format!("Could not decode image: {error}"))
}

fn has_supported_image_signature(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP")
        || (bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif")
        || (bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avis")
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
    prefix.contains("<svg") || prefix.contains("<?xml") && prefix.contains("<svg")
}

fn decode_svg(bytes: &[u8], max_pixels: u64) -> Result<RgbaImage, String> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default())
        .map_err(|error| format!("Could not parse SVG image: {error}"))?;
    let size = tree.size();
    let width = size.width().ceil() as u32;
    let height = size.height().ceil() as u32;
    if width == 0 || height == 0 {
        return Err("SVG image has invalid dimensions.".to_owned());
    }
    if u64::from(width).saturating_mul(u64::from(height)) > max_pixels {
        return Err("SVG image is too large to display safely.".to_owned());
    }
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "Could not allocate SVG image pixels.".to_owned())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    RgbaImage::from_raw(width, height, pixmap.take())
        .ok_or_else(|| "Could not convert SVG pixels.".to_owned())
}

fn unrecognized_image_response(url: &str, bytes: &[u8]) -> String {
    let body = String::from_utf8_lossy(&bytes[..bytes.len().min(512)])
        .trim_start()
        .to_ascii_lowercase();
    let location = Url::parse(url)
        .ok()
        .map(|url| format!("{}{}", url.host_str().unwrap_or("image host"), url.path()))
        .unwrap_or_else(|| "unknown image URL".to_owned());
    if body.starts_with("<!doctype html")
        || body.starts_with("<html")
        || body.starts_with("<?xml")
        || body.starts_with("{")
    {
        format!(
            "The server returned a web/error document instead of a supported image ({location})."
        )
    } else {
        format!("Unsupported or unknown image format ({location}); supported: PNG, JPEG, GIF, WebP, SVG.")
    }
}

async fn search_searxng(
    backend: &dyn NetworkAdapter,
    query: &str,
) -> Result<Vec<SearchResult>, String> {
    let endpoint = match std::env::var("NYX_SEARXNG_URL") {
        Ok(endpoint) => endpoint,
        Err(std::env::VarError::NotPresent) => default_searxng_endpoint().to_owned(),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("NYX_SEARXNG_URL must be valid Unicode".to_owned())
        }
    };
    let url = searxng_search_url(&endpoint, query)
        .map_err(|error| format!("Invalid SearXNG endpoint: {error}"))?;
    let html = backend
        .fetch(&url)
        .await
        .map_err(|error| format!("SearXNG request failed: {error}"))?;
    parse_searxng_results(&html).map_err(|error| {
        format!("SearXNG returned an invalid response (is JSON output enabled?): {error}")
    })
}

fn youtube_search_query(query: &str) -> Option<&str> {
    let (prefix, search) = query.trim().split_at_checked(3)?;
    if prefix.eq_ignore_ascii_case("yt:") {
        let search = search.trim();
        (!search.is_empty()).then_some(search)
    } else {
        None
    }
}

async fn search_youtube_videos(query: &str) -> Result<Vec<SearchResult>, String> {
    if query.trim().is_empty() {
        return Err("Enter a YouTube search term after `yt:`.".to_owned());
    }

    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("yt-dlp")
            .args([
                "--flat-playlist",
                "--dump-single-json",
                "--no-warnings",
                "--socket-timeout",
                "15",
                "--playlist-end",
                "10",
            ])
            .arg(format!("ytsearch10:{query}"))
            .output(),
    )
    .await
    .map_err(|_| "YouTube search timed out after 30 seconds.".to_owned())?
    .map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            "yt-dlp is not installed or is not on PATH; install it to search YouTube videos."
                .to_owned()
        } else {
            format!("Could not start yt-dlp: {error}")
        }
    })?;
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr);
        let details = details.trim();
        return Err(if details.is_empty() {
            format!("yt-dlp exited with status {}.", output.status)
        } else {
            format!("YouTube search failed: {details}")
        });
    }

    parse_youtube_search_results(&output.stdout)
}

fn parse_youtube_search_results(json: &[u8]) -> Result<Vec<SearchResult>, String> {
    let response: serde_json::Value = serde_json::from_slice(json)
        .map_err(|error| format!("yt-dlp returned invalid search data: {error}"))?;
    let entries = response
        .get("entries")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "yt-dlp returned no video results.".to_owned())?;

    let results = entries
        .iter()
        .filter_map(|entry| {
            let title = entry.get("title")?.as_str()?.trim();
            if title.is_empty() {
                return None;
            }
            let url = entry
                .get("webpage_url")
                .and_then(serde_json::Value::as_str)
                .filter(|url| is_youtube_video_url(url))
                .map(str::to_owned)
                .or_else(|| {
                    let id = entry.get("id")?.as_str()?;
                    if id.is_empty()
                        || !id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                    {
                        return None;
                    }
                    Some(format!("https://www.youtube.com/watch?v={id}"))
                })?;
            let snippet = entry
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("YouTube video")
                .to_owned();
            Some(SearchResult {
                title: title.to_owned(),
                url,
                snippet,
            })
        })
        .collect::<Vec<_>>();
    if results.is_empty() {
        Err("No YouTube videos were found for that query.".to_owned())
    } else {
        Ok(results)
    }
}

fn handle_key(
    code: KeyCode,
    state: &mut AppState,
    selected_link: &mut usize,
    show_help: &mut bool,
    bookmark_focus: &mut bool,
) -> bool {
    match code {
        KeyCode::Char('q') => return true,
        KeyCode::Esc => {
            if *show_help {
                *show_help = false;
            } else {
                state.go_back();
            }
        }
        KeyCode::F(1) => *show_help = !*show_help,
        KeyCode::Tab => match &state.view {
            AppView::Home => *bookmark_focus = !*bookmark_focus,
            AppView::SearchResults { links, .. } => {
                if let Some(next) = cycle_link(*selected_link, links.len(), true) {
                    *selected_link = next;
                    state.result_page = *selected_link / RESULTS_PER_PAGE;
                }
            }
            AppView::ArticleReader { links, .. } => {
                if let Some(next) = cycle_link(*selected_link, links.len(), true) {
                    *selected_link = next;
                }
            }
        },
        KeyCode::BackTab => match &state.view {
            AppView::Home => *bookmark_focus = !*bookmark_focus,
            AppView::SearchResults { links, .. } => {
                if let Some(previous) = cycle_link(*selected_link, links.len(), false) {
                    *selected_link = previous;
                    state.result_page = *selected_link / RESULTS_PER_PAGE;
                }
            }
            AppView::ArticleReader { links, .. } => {
                if let Some(previous) = cycle_link(*selected_link, links.len(), false) {
                    *selected_link = previous;
                }
            }
        },
        KeyCode::PageUp => {
            if matches!(state.view, AppView::SearchResults { .. }) {
                state.result_page = state.result_page.saturating_sub(1);
                *selected_link = state.result_page * RESULTS_PER_PAGE;
            } else {
                state.scroll = state.scroll.saturating_sub(10);
            }
        }
        KeyCode::PageDown => {
            if let AppView::SearchResults { links, .. } = &state.view {
                let last_page = links.len().saturating_sub(1) / RESULTS_PER_PAGE;
                state.result_page = (state.result_page + 1).min(last_page);
                *selected_link = state.result_page * RESULTS_PER_PAGE;
            } else {
                state.scroll = state.scroll.saturating_add(10);
            }
        }
        KeyCode::Up => state.scroll = state.scroll.saturating_sub(1),
        KeyCode::Down => state.scroll = state.scroll.saturating_add(1),
        KeyCode::Char(character @ '1'..='9') if !matches!(state.view, AppView::Home) => {
            let index = usize::from(character as u8 - b'1');
            if let AppView::SearchResults { links, .. } = &state.view {
                let absolute_index = state
                    .result_page
                    .saturating_mul(RESULTS_PER_PAGE)
                    .saturating_add(index);
                if absolute_index < links.len() {
                    *selected_link = absolute_index;
                }
            } else if let AppView::ArticleReader { links, .. } = &state.view {
                if index < links.len() {
                    *selected_link = index;
                }
            }
        }
        KeyCode::Backspace => {
            if *bookmark_focus && matches!(state.view, AppView::Home) {
                state.bookmark_input.pop();
            } else {
                state.input.pop();
            }
        }
        KeyCode::Char(character) => {
            if *bookmark_focus && matches!(state.view, AppView::Home) {
                state.bookmark_input.push(character);
            } else {
                state.input.push(character);
            }
        }
        _ => {}
    }
    false
}

fn render(
    frame: &mut ratatui::Frame<'_>,
    state: &AppState,
    metrics: &SystemMetrics,
    context: UiContext<'_>,
    link_hitboxes: &mut Vec<LinkHitbox>,
) {
    link_hitboxes.clear();
    let UiContext {
        selected_link,
        show_help,
        notice,
        modal,
        tor_configured,
        tor_connected,
    } = context;
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(frame.size());
    let control_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(areas[2]);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " Nyx ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("URL / Search: "),
        Span::styled(
            if state.input.is_empty() {
                "type a search and press Enter"
            } else {
                &state.input
            },
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  [Ctrl+T] Tor: "),
        Span::styled(
            if tor_connected {
                "CONNECTED"
            } else if tor_configured {
                "DISCONNECTED"
            } else {
                "DIRECT"
            },
            Style::default().fg(if tor_connected {
                Color::Green
            } else if tor_configured {
                Color::Red
            } else {
                Color::DarkGray
            }),
        ),
        Span::raw("]"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, areas[0]);

    match &state.view {
        AppView::Home => {
            let home = Paragraph::new(vec![
                Line::from(Span::styled(
                    "Welcome to Nyx",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("Enter a search or URL in the header input and press Enter."),
                // Line::from("Search YouTube videos with: yt: your search terms"),
                Line::from(""),
                Line::from("Quick bookmarks: enter a URL below and press Enter to open it."),
                Line::from(format!(
                    "Quick bookmark URL: {}",
                    if state.bookmark_input.is_empty() {
                        "Tab to enter a URL"
                    } else {
                        &state.bookmark_input
                    }
                )),
            ])
            .block(Block::default().title("Home").borders(Borders::ALL))
            .wrap(Wrap { trim: true });
            frame.render_widget(home, areas[1]);
        }
        AppView::SearchResults { query, links } => {
            let start = state.result_page.saturating_mul(RESULTS_PER_PAGE);
            let end = start.saturating_add(RESULTS_PER_PAGE).min(links.len());
            let page_links = links.get(start..end).unwrap_or_default();
            let items = if links.is_empty() {
                let message = if notice.is_some_and(|message| message.starts_with("Searching ")) {
                    format!("Searching for \"{query}\"...")
                } else {
                    format!("No results found for \"{query}\".")
                };
                vec![ListItem::new(message)]
            } else {
                page_links
                    .iter()
                    .enumerate()
                    .map(|(index, result)| {
                        ListItem::new(format!(
                            "[{}] {} — {}\n    {}",
                            start + index + 1,
                            result.title,
                            result.url,
                            result.snippet
                        ))
                    })
                    .collect()
            };
            let mut list_state = ListState::default();
            if !page_links.is_empty() {
                list_state.select(Some(
                    selected_link
                        .saturating_sub(start)
                        .min(page_links.len() - 1),
                ));
            }
            let results = List::new(items)
                .block(
                    Block::default()
                        .title(format!(
                            "Search: {query}  Page {}/{}",
                            state.result_page + 1,
                            links.len().max(1).div_ceil(RESULTS_PER_PAGE)
                        ))
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
            frame.render_stateful_widget(results, areas[1], &mut list_state);
        }
        AppView::ArticleReader {
            title,
            links,
            images,
            forms,
            reader,
            ..
        } => {
            let lines = render_reader_document(reader, selected_link);
            let focused_link = links.get(selected_link).map_or_else(
                || "[f] Fill form".to_owned(),
                |url| {
                    format!(
                        "Link {} of {}: {url}  [Tab] Next  [Shift+Tab] Previous  [Enter] Open  [f] Form  [Ctrl+T] Tor",
                        selected_link + 1,
                        links.len()
                    )
                },
            );
            let focused_link = format!("{focused_link}  [i] Read images ({})", images.len());
            let article = Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(format!("{title}  [f] Fill form ({})", forms.len()))
                        .borders(Borders::ALL)
                        .padding(Padding::horizontal(2)),
                )
                .wrap(Wrap { trim: true })
                .scroll((state.scroll, 0));
            frame.render_widget(article, areas[1]);
            *link_hitboxes =
                collect_link_hitboxes(frame.buffer_mut(), areas[1], links.len(), selected_link);
            frame.render_widget(Paragraph::new(focused_link), control_areas[0]);
        }
    }

    if matches!(modal.as_ref(), Some(ActiveModal::Image(_))) {
        frame.render_widget(
            Paragraph::new("[Left/Right] Previous/next readable panel  [Esc] Close reader"),
            control_areas[0],
        );
    } else if !matches!(state.view, AppView::ArticleReader { .. }) {
        let controls = if matches!(state.view, AppView::Home) {
            "[Enter] Search/open  [Tab] Bookmark input  [Ctrl+V] Paste  [Ctrl+T] Tor  [F1] Help  [q] Quit"
        } else {
            "[↑/↓] Select  [Enter] Open  [PgUp/PgDn] Page/scroll  [Tab] Next link  [i] Images  [F1] Help  [q] Quit"
        };
        frame.render_widget(Paragraph::new(controls), control_areas[0]);
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[OK]", Style::default().fg(Color::Green)),
            Span::raw(" Success  "),
            Span::styled("[WAIT/WARN]", Style::default().fg(Color::Yellow)),
            Span::raw(" Loading / warning  "),
            Span::styled("[ERROR]", Style::default().fg(Color::Red)),
            Span::raw(" Failure"),
        ])),
        control_areas[1],
    );
    let metrics_status = format_metrics(Some(metrics));
    let status = if let Some(message) = notice {
        let (label, color) = status_indicator(message);
        Line::from(vec![
            Span::styled(
                format!("{label} {message}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  |  "),
            Span::styled(metrics_status, Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(Span::styled(
            metrics_status,
            Style::default().fg(Color::DarkGray),
        ))
    };
    frame.render_widget(Paragraph::new(status), areas[3]);
    if show_help {
        let area = centered_rect(60, 40, frame.size());
        frame.render_widget(
            Paragraph::new(
                "Keyboard: Enter searches or opens the selected link. Ctrl+V pastes on Home; Ctrl+L returns Home; Ctrl+T toggles Tor. Up/Down selects results or scrolls; PgUp/PgDn changes pages or scrolls. Tab/Shift+Tab cycles links and form fields; 1-9 selects a result/link. Press i to view article images (Left/Right changes image, Esc closes), f to fill a form, F1 for help, and q/Ctrl+C to quit.\nStatus colors: green [OK] means success; yellow [WAIT/WARN] means loading or warning; red [ERROR] means failure.\nYouTube text search uses \"yt: words\"; video playback is not supported. Tor must already be running; Nyx blocks network requests if its configured Tor route is unavailable.\nMouse: left-click a search result or rendered article link to open it; use the wheel to scroll.",
            )
            .block(Block::default().title("Nyx Help").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            area,
        );
    }
    match modal {
        Some(ActiveModal::Form(form_session)) => render_form(frame, form_session),
        Some(ActiveModal::Image(viewer)) => {
            if let AppView::ArticleReader { images, .. } = &state.view {
                if let Some(image) = images.get(viewer.index) {
                    render_image_viewer(frame, viewer, image, images.len(), areas[1]);
                }
            }
        }
        None => {}
    }
}

fn status_indicator(message: &str) -> (String, Color) {
    if message.starts_with("Loading ") || message.starts_with("Searching ") {
        let frames = ['|', '/', '-', '\\'];
        let frame = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| (time.as_millis() / 120) as usize % frames.len());
        return (format!("[{}]", frames[frame]), Color::Yellow);
    }

    let lowercase = message.to_ascii_lowercase();
    if [
        "failed",
        "error",
        "invalid",
        "could not",
        "blocked",
        "refusing",
        "too large",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
    {
        return ("[ERROR]".to_owned(), Color::Red);
    }
    if message == "Page loaded."
        || message == "Form submitted."
        || message == "Tor connected and verified."
    {
        return ("[OK]".to_owned(), Color::Green);
    }
    if lowercase.contains("ignored a malformed")
        || lowercase.contains("ignored a non-text")
        || lowercase.contains("skipped")
        || lowercase.contains("warning")
    {
        return ("[WARN]".to_owned(), Color::Yellow);
    }
    ("[INFO]".to_owned(), Color::Cyan)
}

fn render_image_viewer(
    frame: &mut ratatui::Frame<'_>,
    viewer: &ImageViewer,
    reference: &ArticleImage,
    image_count: usize,
    area: ratatui::layout::Rect,
) {
    let title = if reference.alt.is_empty() {
        format!(
            "Image {} of {} — {} — Left/Right: change, Esc: close",
            viewer.index + 1,
            image_count,
            reference.url
        )
    } else {
        format!(
            "Image {} of {} — {} — Left/Right: change, Esc: close",
            viewer.index + 1,
            image_count,
            reference.alt
        )
    };
    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(Color::Cyan))
        .style(Style::default().fg(Color::White).bg(Color::Black));
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let image_area = ratatui::layout::Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(1),
    );
    render_image_pixels(frame, &viewer.image, image_area);
}

fn render_image_pixels(
    frame: &mut ratatui::Frame<'_>,
    image: &RgbaImage,
    inner: ratatui::layout::Rect,
) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let source_width = f64::from(image.width());
    let source_height = f64::from(image.height());
    let available_pixel_width = u32::from(inner.width).saturating_mul(2);
    let available_pixel_height = u32::from(inner.height).saturating_mul(4);
    let scale = (f64::from(available_pixel_width) / source_width)
        .min(f64::from(available_pixel_height) / source_height);
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let target_width = (source_width * scale)
        .floor()
        .max(1.0)
        .min(f64::from(available_pixel_width)) as u32;
    let target_height = (source_height * scale)
        .floor()
        .max(1.0)
        .min(f64::from(available_pixel_height)) as u32;
    let resized = image::imageops::resize(
        image,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    let offset_x = (available_pixel_width - target_width) / 2;
    let offset_y = (available_pixel_height - target_height) / 2;
    let lines = (0..inner.height)
        .map(|row| {
            let spans = (0..inner.width)
                .map(|column| {
                    let mut pixels = [[0u8; 4]; 8];
                    for pixel_row in 0..4u32 {
                        for pixel_column in 0..2u32 {
                            let cell_index = (pixel_row * 2 + pixel_column) as usize;
                            let x = u32::from(column) * 2 + pixel_column;
                            let y = u32::from(row) * 4 + pixel_row;
                            if x >= offset_x
                                && y >= offset_y
                                && x - offset_x < target_width
                                && y - offset_y < target_height
                            {
                                pixels[cell_index] =
                                    resized.get_pixel(x - offset_x, y - offset_y).0;
                            }
                        }
                    }

                    let (glyph, foreground, background) = braille_cell(pixels);
                    Span::styled(
                        glyph.to_string(),
                        Style::default().fg(foreground).bg(background),
                    )
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(Color::Black)),
        inner,
    );
}

const BRAILLE_DOT_BITS: [u8; 8] = [1, 8, 2, 16, 4, 32, 64, 128];

fn braille_cell(pixels: [[u8; 4]; 8]) -> (char, Color, Color) {
    let luminance = pixels.map(pixel_luminance);
    let min = luminance.iter().copied().fold(u8::MAX, u8::min);
    let max = luminance.iter().copied().fold(u8::MIN, u8::max);
    let threshold = (u16::from(min) + u16::from(max)) / 2;
    let mut mask = 0u8;
    let mut dark_pixels = Vec::new();
    let mut light_pixels = Vec::new();
    for index in 0..pixels.len() {
        if u16::from(luminance[index]) >= threshold {
            mask |= BRAILLE_DOT_BITS[index];
            light_pixels.push(pixels[index]);
        } else {
            dark_pixels.push(pixels[index]);
        }
    }
    if light_pixels.is_empty() {
        let background = average_pixel_color(&dark_pixels);
        return (' ', Color::Black, background);
    }
    if dark_pixels.is_empty() {
        let foreground = average_pixel_color(&light_pixels);
        return ('█', foreground, foreground);
    }
    let glyph = char::from_u32(0x2800 + u32::from(mask)).unwrap_or('\u{2800}');
    (
        glyph,
        average_pixel_color(&light_pixels),
        average_pixel_color(&dark_pixels),
    )
}

fn pixel_luminance(pixel: [u8; 4]) -> u8 {
    let color = pixel_color(pixel);
    if let Color::Rgb(red, green, blue) = color {
        ((u32::from(red) * 299 + u32::from(green) * 587 + u32::from(blue) * 114) / 1000) as u8
    } else {
        0
    }
}

fn average_pixel_color(pixels: &[[u8; 4]]) -> Color {
    let count = pixels.len() as u32;
    let mut red = 0u32;
    let mut green = 0u32;
    let mut blue = 0u32;
    for pixel in pixels {
        if let Color::Rgb(pixel_red, pixel_green, pixel_blue) = pixel_color(*pixel) {
            red += u32::from(pixel_red);
            green += u32::from(pixel_green);
            blue += u32::from(pixel_blue);
        }
    }
    Color::Rgb(
        (red / count) as u8,
        (green / count) as u8,
        (blue / count) as u8,
    )
}

fn pixel_color(pixel: [u8; 4]) -> Color {
    let alpha = u16::from(pixel[3]);
    Color::Rgb(
        (u16::from(pixel[0]) * alpha / 255) as u8,
        (u16::from(pixel[1]) * alpha / 255) as u8,
        (u16::from(pixel[2]) * alpha / 255) as u8,
    )
}

fn render_form(frame: &mut ratatui::Frame<'_>, session: &FormSession) {
    let area = centered_rect(72, 70, frame.size());
    let fields = session
        .form
        .fields
        .iter()
        .zip(&session.values)
        .enumerate()
        .filter(|(_, (field, _))| field.kind != FormFieldKind::Hidden)
        .map(|(index, (field, value))| {
            let display_value = if field.kind == FormFieldKind::Password {
                "*".repeat(value.chars().count())
            } else if matches!(
                field.kind,
                FormFieldKind::Choice | FormFieldKind::Checkbox | FormFieldKind::Radio
            ) {
                field
                    .choices
                    .iter()
                    .find(|(choice, _)| choice == value)
                    .map(|(_, label)| label.as_str())
                    .unwrap_or(value)
                    .to_owned()
            } else {
                value.clone()
            };
            let required = if field.required { " *" } else { "" };
            let style = if index == session.active_field {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{}{}: {}", field.label, required, display_value),
                style,
            ))
        })
        .collect::<Vec<_>>();
    let mut lines = fields;
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "[Tab/Shift+Tab] Field  [Enter] Next/Submit  [Ctrl+Enter] Submit  [Esc] Cancel  {}",
        session.form.submit_label
    )));
    let hidden_count = session
        .form
        .fields
        .iter()
        .filter(|field| field.kind == FormFieldKind::Hidden)
        .count();
    if hidden_count > 0 {
        lines.push(Line::from(format!(
            "{hidden_count} hidden field(s), such as CSRF tokens, will be submitted."
        )));
    }
    let modal_style = Style::default().fg(Color::White).bg(Color::Black);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(modal_style)
            .block(
                Block::default()
                    .title(format!("Web form — {}", session.form.action))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .style(modal_style),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn cycle_link(current: usize, link_count: usize, forward: bool) -> Option<usize> {
    if link_count == 0 {
        None
    } else if forward {
        Some(current.saturating_add(1) % link_count)
    } else {
        let current = current % link_count;
        Some(if current == 0 {
            link_count - 1
        } else {
            current - 1
        })
    }
}

fn link_marker_background(link_index: usize) -> Option<Color> {
    let encoded = u32::try_from(link_index).ok()?.checked_add(1)?;
    (encoded <= 0x00ff_ffff).then_some(Color::Rgb(
        ((encoded >> 16) & 0xff) as u8,
        ((encoded >> 8) & 0xff) as u8,
        (encoded & 0xff) as u8,
    ))
}

fn link_index_from_marker(color: Color, link_count: usize) -> Option<usize> {
    let Color::Rgb(red, green, blue) = color else {
        return None;
    };
    let encoded = (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue);
    let index = usize::try_from(encoded.checked_sub(1)?).ok()?;
    (index < link_count).then_some(index)
}

fn hit_test_link(hitboxes: &[LinkHitbox], column: u16, row: u16) -> Option<usize> {
    hitboxes
        .iter()
        .find(|hitbox| {
            hitbox.row == row && column >= hitbox.column_start && column < hitbox.column_end
        })
        .map(|hitbox| hitbox.link_index)
}

fn collect_link_hitboxes(
    buffer: &mut Buffer,
    area: ratatui::layout::Rect,
    link_count: usize,
    selected_link: usize,
) -> Vec<LinkHitbox> {
    let mut hitboxes = Vec::new();
    for row in area.y..area.y.saturating_add(area.height) {
        let mut current: Option<LinkHitbox> = None;
        for column in area.x..area.x.saturating_add(area.width) {
            let cell = buffer.get_mut(column, row);
            let link_index = link_index_from_marker(cell.bg, link_count);
            if let Some(link_index) = link_index {
                cell.set_bg(if link_index == selected_link {
                    Color::Yellow
                } else {
                    Color::Reset
                });
            }

            match (current.as_mut(), link_index) {
                (Some(hitbox), Some(link_index))
                    if hitbox.link_index == link_index && hitbox.column_end == column =>
                {
                    hitbox.column_end = column.saturating_add(1);
                }
                _ => {
                    if let Some(hitbox) = current.take() {
                        hitboxes.push(hitbox);
                    }
                    if let Some(link_index) = link_index {
                        current = Some(LinkHitbox {
                            link_index,
                            row,
                            column_start: column,
                            column_end: column.saturating_add(1),
                        });
                    }
                }
            }
        }
        if let Some(hitbox) = current {
            hitboxes.push(hitbox);
        }
    }
    hitboxes
}

fn render_reader_document(document: &ReaderDocument, selected_link: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(byline) = &document.byline {
        lines.push(Line::from(Span::styled(
            byline.clone(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(""));
    }

    for (block_index, block) in document.blocks.iter().enumerate() {
        match block {
            ReaderBlock::Heading { level, content } => {
                let style = match level {
                    1 => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    2 => Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                };
                lines.push(Line::from(render_reader_inlines(
                    content,
                    selected_link,
                    style,
                )));
                lines.push(Line::from(""));
            }
            ReaderBlock::Paragraph(content) => {
                lines.push(Line::from(render_reader_inlines(
                    content,
                    selected_link,
                    Style::default().fg(Color::White),
                )));
                lines.push(Line::from(""));
            }
            ReaderBlock::Blockquote(content) => {
                let mut spans = vec![Span::styled(
                    "│ ",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(render_reader_inlines(
                    content,
                    selected_link,
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::ITALIC),
                ));
                lines.push(Line::from(spans));
                lines.push(Line::from(""));
            }
            ReaderBlock::List { ordered, items } => {
                for (item_index, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("  {}. ", item_index + 1)
                    } else {
                        "  • ".to_owned()
                    };
                    let mut spans = vec![Span::styled(
                        marker,
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )];
                    spans.extend(render_reader_inlines(
                        item,
                        selected_link,
                        Style::default().fg(Color::White),
                    ));
                    lines.push(Line::from(spans));
                }
                lines.push(Line::from(""));
            }
            ReaderBlock::LinkList(items) => {
                for item in items {
                    lines.push(render_reader_link_item(item, selected_link));
                }
                lines.push(Line::from(""));
            }
            ReaderBlock::Code(code) => {
                for code_line in code.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  {code_line}"),
                        Style::default().fg(Color::LightYellow),
                    )));
                }
                lines.push(Line::from(""));
            }
        }

        if block_index + 1 == document.blocks.len() {
            while lines.last().is_some_and(|line| line.spans.is_empty()) {
                lines.pop();
            }
        }
    }
    lines
}

fn render_reader_link_item(item: &ReaderLinkItem, selected_link: usize) -> Line<'static> {
    let marker_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let marker_background = link_marker_background(item.link_index).unwrap_or(Color::Reset);
    let title_style = if item.link_index == selected_link {
        Style::default()
            .fg(Color::Black)
            .bg(marker_background)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(Color::LightBlue)
            .bg(marker_background)
            .add_modifier(Modifier::UNDERLINED)
    };
    Line::from(vec![
        Span::styled(format!("[{}] ", item.link_index + 1), marker_style),
        Span::styled(item.text.clone(), title_style),
    ])
}

fn render_reader_inlines(
    content: &[ReaderInline],
    selected_link: usize,
    text_style: Style,
) -> Vec<Span<'static>> {
    content
        .iter()
        .flat_map(|inline| match inline {
            ReaderInline::Text(text) => vec![Span::styled(text.clone(), text_style)],
            ReaderInline::Code(code) => vec![Span::styled(
                code.clone(),
                Style::default().fg(Color::LightYellow).bg(Color::DarkGray),
            )],
            ReaderInline::Link { text, link_index } => {
                let marker_background = link_marker_background(*link_index).unwrap_or(Color::Reset);
                let link_style = if *link_index == selected_link {
                    Style::default()
                        .fg(Color::Black)
                        .bg(marker_background)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default()
                        .fg(Color::LightBlue)
                        .bg(marker_background)
                        .add_modifier(Modifier::UNDERLINED)
                };
                vec![
                    Span::styled(text.clone(), link_style),
                    Span::styled(
                        format!(" [Link #{}]", link_index + 1),
                        Style::default()
                            .fg(Color::Cyan)
                            .bg(marker_background)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            }
        })
        .collect()
}

fn centered_rect(width: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

fn format_metrics(metrics: Option<&SystemMetrics>) -> String {
    let Some(metrics) = metrics else {
        return "RAM: N/A  Zpool LZ4: N/A  Active core: N/A".to_owned();
    };
    let ram = match (metrics.ram_used_bytes, metrics.ram_total_bytes) {
        (Some(used), Some(total)) => format!(
            "RAM: {:.1}/{:.1} GiB",
            used as f64 / 1_073_741_824.0,
            total as f64 / 1_073_741_824.0
        ),
        _ => "RAM: N/A".to_owned(),
    };
    let zpool = metrics
        .zpool_lz4_ratio
        .map(|ratio| format!("Zpool LZ4: {ratio:.2}x"))
        .unwrap_or_else(|| "Zpool LZ4: N/A".to_owned());
    let core = metrics
        .active_core
        .map(|core| format!("Active core: {core}"))
        .unwrap_or_else(|| "Active core: N/A".to_owned());
    format!("{ram}  {zpool}  {core}")
}

#[cfg(test)]
mod tests {
    use super::{
        append_pasted_text, braille_cell, collect_link_hitboxes, cycle_link, decode_image,
        handle_key, hit_test_link, image_indices, is_current_navigation, is_youtube_video_url,
        link_marker_background, next_form_field, parse_youtube_search_results,
        render_reader_document, search_searxng, status_indicator, validate_form,
        youtube_search_query, FormSession, BRAILLE_DOT_BITS,
    };
    use crossterm::event::KeyCode;
    use nyx_core::{
        AppState, FormField, FormFieldKind, FormMethod, HtmlForm, ReaderBlock, ReaderDocument,
        ReaderInline, ReaderLinkItem,
    };
    use nyx_pal::{AdapterError, AdapterFuture, FormSubmission, NetworkAdapter, PageResponse};
    use ratatui::style::{Color, Modifier};
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn status_indicators_distinguish_loading_success_and_errors() {
        let (loading, loading_color) = status_indicator("Loading https://example.com");
        assert!(["[|]", "[/]", "[-]", "[\\]"].contains(&loading.as_str()));
        assert_eq!(loading_color, Color::Yellow);
        assert_eq!(
            status_indicator("Page loaded."),
            ("[OK]".to_owned(), Color::Green)
        );
        assert_eq!(
            status_indicator("Page failed: timeout"),
            ("[ERROR]".to_owned(), Color::Red)
        );
        assert_eq!(
            status_indicator("ignored a malformed Set-Cookie header from the server"),
            ("[WARN]".to_owned(), Color::Yellow)
        );
    }

    #[test]
    fn recognizes_supported_youtube_video_urls_only() {
        assert!(is_youtube_video_url(
            "https://www.youtube.com/watch?v=video-id"
        ));
        assert!(is_youtube_video_url("https://youtu.be/video-id"));
        assert!(is_youtube_video_url(
            "https://m.youtube.com/shorts/video-id"
        ));
        assert!(is_youtube_video_url("https://youtube.com/live/video-id"));
        assert!(!is_youtube_video_url("https://www.youtube.com/"));
        assert!(!is_youtube_video_url(
            "https://example.com/watch?v=video-id"
        ));
        assert!(!is_youtube_video_url(
            "file:///youtube.com/watch?v=video-id"
        ));
    }

    #[test]
    fn identifies_youtube_search_prefix_and_builds_playable_result_urls() {
        assert_eq!(
            youtube_search_query("yt: rust language"),
            Some("rust language")
        );
        assert_eq!(
            youtube_search_query("YT:  nyx browser  "),
            Some("nyx browser")
        );
        assert_eq!(youtube_search_query("youtube video"), None);
        assert_eq!(youtube_search_query("yt: "), None);

        let results = match parse_youtube_search_results(
            br#"{"entries":[{"id":"abc_123","title":"A video","description":"A snippet"},{"id":"bad/id","title":"Unsafe"}]}"#,
        ) {
            Ok(results) => results,
            Err(error) => panic!("YouTube results failed to parse: {error}"),
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "A video");
        assert_eq!(results[0].url, "https://www.youtube.com/watch?v=abc_123");
        assert_eq!(results[0].snippet, "A snippet");
    }

    #[test]
    fn ignores_page_completions_from_previous_navigation() {
        assert!(is_current_navigation(4, 4));
        assert!(!is_current_navigation(3, 4));
    }

    #[test]
    fn pasted_terminal_text_is_added_without_control_characters() {
        let mut input = String::new();
        append_pasted_text(&mut input, "https://youtu.be/abc\r\n");
        assert_eq!(input, "https://youtu.be/abc");
    }

    #[test]
    fn reader_mode_renders_structured_blocks_and_selected_inline_links() {
        let document = ReaderDocument {
            title: "Article".to_owned(),
            byline: Some("By Nyx".to_owned()),
            links: vec!["https://example.test".to_owned()],
            blocks: vec![
                ReaderBlock::Heading {
                    level: 1,
                    content: vec![ReaderInline::Text("Chapter title".to_owned())],
                },
                ReaderBlock::Paragraph(vec![
                    ReaderInline::Text("Read ".to_owned()),
                    ReaderInline::Link {
                        text: "this".to_owned(),
                        link_index: 0,
                    },
                    ReaderInline::Code("<p>".to_owned()),
                ]),
                ReaderBlock::List {
                    ordered: false,
                    items: vec![vec![ReaderInline::Text("First item".to_owned())]],
                },
                ReaderBlock::LinkList(vec![ReaderLinkItem {
                    text: "A story title".to_owned(),
                    link_index: 1,
                }]),
            ],
        };
        let lines = render_reader_document(&document, 0);

        assert_eq!(lines[0].spans[0].content, "By Nyx");
        assert_eq!(lines[2].spans[0].content, "Chapter title");
        assert!(lines[2].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(lines[4].spans[1].content, "this");
        assert_eq!(lines[4].spans[1].style.bg, link_marker_background(0));
        assert_eq!(lines[4].spans[2].content, " [Link #1]");
        assert_eq!(lines[4].spans[3].content, "<p>");
        assert_eq!(lines[4].spans[3].style.fg, Some(Color::LightYellow));
        assert!(lines.iter().any(|line| line
            .spans
            .iter()
            .any(|span| span.content.contains("First item"))));
        let index_link = lines
            .iter()
            .find(|line| {
                line.spans
                    .first()
                    .is_some_and(|span| span.content == "[2] ")
            })
            .expect("index link should render as a numbered list item");
        assert_eq!(index_link.spans[1].content, "A story title");
        assert_eq!(index_link.spans[1].style.fg, Some(Color::LightBlue));
    }

    #[test]
    fn number_keys_are_inserted_into_home_search_and_bookmark_inputs() {
        let mut state = AppState::default();
        let mut selected_link = 0;
        let mut show_help = false;
        let mut bookmark_focus = false;

        handle_key(
            KeyCode::Char('3'),
            &mut state,
            &mut selected_link,
            &mut show_help,
            &mut bookmark_focus,
        );
        assert_eq!(state.input, "3");

        bookmark_focus = true;
        handle_key(
            KeyCode::Char('7'),
            &mut state,
            &mut selected_link,
            &mut show_help,
            &mut bookmark_focus,
        );
        assert_eq!(state.bookmark_input, "7");
    }

    #[test]
    fn braille_image_renderer_maps_all_eight_dots() {
        assert_eq!(BRAILLE_DOT_BITS, [1, 8, 2, 16, 4, 32, 64, 128]);
        let black = [0, 0, 0, 255];
        let white = [255, 255, 255, 255];
        for (index, bit) in BRAILLE_DOT_BITS.iter().enumerate() {
            let mut pixels = [black; 8];
            pixels[index] = white;
            let (glyph, foreground, background) = braille_cell(pixels);
            assert_eq!(glyph as u32, 0x2800 + u32::from(*bit));
            assert_eq!(foreground, Color::Rgb(255, 255, 255));
            assert_eq!(background, Color::Rgb(0, 0, 0));
        }
        assert_eq!(
            braille_cell([white; 8]),
            ('█', Color::Rgb(255, 255, 255), Color::Rgb(255, 255, 255))
        );
        assert_eq!(
            braille_cell([black; 8]),
            ('█', Color::Rgb(0, 0, 0), Color::Rgb(0, 0, 0))
        );
    }

    #[test]
    fn decodes_inline_svg_and_reports_non_image_responses() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="3"><rect width="4" height="3" fill="#ff0000"/></svg>"##;
        let image = match decode_image("https://example.test/logo.svg", svg, 100) {
            Ok(image) => image,
            Err(error) => panic!("SVG decoding failed: {error}"),
        };
        assert_eq!(image.dimensions(), (4, 3));
        assert_eq!(
            decode_image(
                "https://example.test/image",
                b"<!doctype html><title>Denied</title>",
                100,
            ),
            Err(
                "The server returned a web/error document instead of a supported image (example.test/image)."
                    .to_owned()
            )
        );
        assert_eq!(
            decode_image(
                "https://example.test/invalid.png",
                b"not actually a png",
                100,
            ),
            Err("Unsupported or unknown image format (example.test/invalid.png); supported: PNG, JPEG, GIF, WebP, SVG.".to_owned())
        );
    }

    #[test]
    fn comic_reader_walks_image_panels_in_both_directions() {
        assert_eq!(image_indices(0, 4, true), [0, 1, 2, 3]);
        assert_eq!(image_indices(3, 4, true), [3, 0, 1, 2]);
        assert_eq!(image_indices(2, 4, false), [2, 1, 0, 3]);
        assert!(image_indices(0, 0, true).is_empty());
    }

    struct MockSearchNetwork {
        requested: AtomicBool,
    }

    impl NetworkAdapter for MockSearchNetwork {
        fn fetch<'a>(&'a self, url: &'a str) -> AdapterFuture<'a, String> {
            self.requested.store(true, Ordering::SeqCst);
            assert!(url.starts_with("http://127.0.0.1:8080/search?"));
            assert!(url.contains("q=nyx"));
            assert!(url.contains("format=json"));
            let body = r#"{"results":[{"title":"Nyx result","url":"https://nyx.test/","content":"Privacy first."}]}"#;
            Box::pin(async move { Ok(body.to_owned()) })
        }

        fn fetch_page<'a>(&'a self, _url: &'a str) -> AdapterFuture<'a, PageResponse> {
            Box::pin(async {
                Err(AdapterError::Vault(
                    "page fetch is not used by this test".to_owned(),
                ))
            })
        }

        fn submit_form<'a>(
            &'a self,
            _submission: &'a FormSubmission,
        ) -> AdapterFuture<'a, PageResponse> {
            Box::pin(async {
                Err(AdapterError::Vault(
                    "form submission is not used by this test".to_owned(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn searches_local_searxng_json_api() {
        let network = MockSearchNetwork {
            requested: AtomicBool::new(false),
        };
        let results = match search_searxng(&network, "nyx").await {
            Ok(results) => results,
            Err(error) => panic!("SearXNG search failed: {error}"),
        };
        assert!(network.requested.load(Ordering::SeqCst));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Nyx result");
    }

    #[test]
    fn enter_submits_a_form_with_only_one_visible_field() {
        let session = FormSession::new(HtmlForm {
            action: "https://nettruyen.example/search".to_owned(),
            method: FormMethod::Get,
            fields: vec![
                FormField {
                    name: "csrf".to_owned(),
                    label: "CSRF".to_owned(),
                    value: "token".to_owned(),
                    kind: FormFieldKind::Hidden,
                    required: false,
                    choices: Vec::new(),
                },
                FormField {
                    name: "keyword".to_owned(),
                    label: "Search...".to_owned(),
                    value: "daemon".to_owned(),
                    kind: FormFieldKind::Text,
                    required: false,
                    choices: Vec::new(),
                },
            ],
            submit_label: "Submit".to_owned(),
        });

        assert_eq!(session.active_field, 1);
        assert_eq!(next_form_field(&session, true), None);
        assert!(validate_form(&session).is_ok());
    }

    #[test]
    fn refuses_password_forms_without_https_post() {
        let make_session = |method, action: &str| {
            FormSession::new(HtmlForm {
                action: action.to_owned(),
                method,
                fields: vec![FormField {
                    name: "password".to_owned(),
                    label: "Password".to_owned(),
                    value: String::new(),
                    kind: FormFieldKind::Password,
                    required: false,
                    choices: Vec::new(),
                }],
                submit_label: "Sign in".to_owned(),
            })
        };

        assert!(
            validate_form(&make_session(FormMethod::Get, "https://example.test/login")).is_err()
        );
        assert!(
            validate_form(&make_session(FormMethod::Post, "http://example.test/login")).is_err()
        );
        assert!(validate_form(&make_session(
            FormMethod::Post,
            "https://example.test/login"
        ))
        .is_ok());
    }

    #[test]
    fn tab_and_shift_tab_cycle_article_links_in_both_directions() {
        let mut state = AppState::default();
        state.open_article(
            "Article".to_owned(),
            "One [Link #1]\nTwo [Link #2]".to_owned(),
            vec!["https://one.test".to_owned(), "https://two.test".to_owned()],
            Vec::new(),
            Vec::new(),
            ReaderDocument {
                title: "Article".to_owned(),
                byline: None,
                blocks: Vec::new(),
                links: vec!["https://one.test".to_owned(), "https://two.test".to_owned()],
            },
        );
        let mut selected_link = 0;
        let mut show_help = false;
        let mut bookmark_focus = false;

        assert!(!handle_key(
            KeyCode::Tab,
            &mut state,
            &mut selected_link,
            &mut show_help,
            &mut bookmark_focus,
        ));
        assert_eq!(selected_link, 1);

        assert!(!handle_key(
            KeyCode::BackTab,
            &mut state,
            &mut selected_link,
            &mut show_help,
            &mut bookmark_focus,
        ));
        assert_eq!(selected_link, 0);

        assert!(!handle_key(
            KeyCode::BackTab,
            &mut state,
            &mut selected_link,
            &mut show_help,
            &mut bookmark_focus,
        ));
        assert_eq!(selected_link, 1);
    }

    #[test]
    fn link_cycling_handles_empty_and_out_of_range_indices() {
        assert_eq!(cycle_link(0, 0, true), None);
        assert_eq!(cycle_link(9, 3, true), Some(1));
        assert_eq!(cycle_link(0, 3, false), Some(2));
    }

    #[test]
    fn mouse_hit_testing_uses_exact_rendered_link_cells() {
        let area = Rect::new(2, 3, 12, 4);
        let mut buffer = Buffer::empty(area);
        let first = link_marker_background(0).expect("first link marker color");
        let second = link_marker_background(1).expect("second link marker color");
        for column in 5..9 {
            buffer.get_mut(column, 4).set_bg(first);
        }
        for column in 8..11 {
            buffer.get_mut(column, 5).set_bg(second);
        }

        let hitboxes = collect_link_hitboxes(&mut buffer, area, 2, 1);
        assert_eq!(hit_test_link(&hitboxes, 5, 4), Some(0));
        assert_eq!(hit_test_link(&hitboxes, 8, 4), Some(0));
        assert_eq!(hit_test_link(&hitboxes, 8, 5), Some(1));
        assert_eq!(hit_test_link(&hitboxes, 11, 5), None);
        assert_eq!(buffer.get(5, 4).bg, Color::Reset);
        assert_eq!(buffer.get(8, 5).bg, Color::Yellow);
    }
}
