use std::{io, time::Duration};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use image::{ImageReader, RgbaImage};
use nyx_core::{
    default_searxng_endpoint, normalize_direct_url, parse_searxng_results, searxng_search_url,
    AppState, AppView, ArticleImage, FormFieldKind, FormMethod, HtmlForm, HtmlParser, SearchResult,
    RESULTS_PER_PAGE,
};
use nyx_pal::{
    FormRequestMethod, FormSubmission, NetworkAdapter, StdBackend, SystemAdapter, SystemMetrics,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use std::io::Cursor;
use tokio::sync::mpsc;
use zeroize::{Zeroize, Zeroizing};

struct SearchCompletion {
    query: String,
    result: Result<Vec<SearchResult>, String>,
}

struct ImageViewer {
    index: usize,
    image: RgbaImage,
}

enum ActiveModal<'a> {
    Form(&'a FormSession),
    Image(&'a ImageViewer),
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
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok((Self, terminal)),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, DisableMouseCapture, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let (backend, _) = StdBackend::open_session_vault(&vault_path, passphrase.as_str())?;

    let (_session, mut terminal) = TerminalSession::enter()?;
    let mut state = AppState::default();
    let mut selected_link = 0usize;
    let mut show_help = false;
    let mut bookmark_focus = false;
    let mut notice = None;
    let mut active_form = None;
    let mut image_viewer: Option<ImageViewer> = None;
    let mut next_form_index = 0usize;
    let (search_tx, mut search_rx) = mpsc::unbounded_channel::<SearchCompletion>();

    loop {
        while let Ok(completion) = search_rx.try_recv() {
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

        let metrics = backend.metrics()?;
        terminal.draw(|frame| {
            render(
                frame,
                &state,
                &metrics,
                selected_link,
                show_help,
                notice.as_deref(),
                active_form
                    .as_ref()
                    .map(ActiveModal::Form)
                    .or_else(|| image_viewer.as_ref().map(ActiveModal::Image)),
            )
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
                        state.go_home();
                        selected_link = 0;
                        bookmark_focus = false;
                        notice = None;
                        active_form = None;
                        image_viewer = None;
                        continue;
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
                                        match load_article_image(&backend, images, next).await {
                                            Ok(viewer) => image_viewer = Some(viewer),
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
                                                        );
                                                        active_form = None;
                                                        selected_link = 0;
                                                        notice = Some("Form submitted.".to_owned());
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
                            match load_article_image(&backend, images, 0).await {
                                Ok(viewer) => {
                                    image_viewer = Some(viewer);
                                    notice = None;
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
                                        notice = Some("Searching local SearXNG...".to_owned());
                                        let query = query.clone();
                                        let backend = backend.clone();
                                        let search_tx = search_tx.clone();
                                        tokio::spawn(async move {
                                            let result = search_searxng(&backend, &query).await;
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
                            notice = open_url(&backend, &mut state, &url).await;
                            selected_link = 0;
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
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        state.scroll = state.scroll.saturating_sub(3);
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll = state.scroll.saturating_add(3);
                    }
                    MouseEventKind::Down(_) => match &state.view {
                        AppView::SearchResults { links, .. } => {
                            let local_row = usize::from(mouse.row.saturating_sub(4));
                            let index = state
                                .result_page
                                .saturating_mul(RESULTS_PER_PAGE)
                                .saturating_add(local_row);
                            if index < links.len() {
                                selected_link = index;
                            }
                        }
                        AppView::ArticleReader { content, links, .. } => {
                            let row = usize::from(mouse.row.saturating_sub(4));
                            if let Some(index) = link_reference_at(content, state.scroll, row) {
                                if index < links.len() {
                                    selected_link = index;
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

fn link_reference_at(content: &str, scroll: u16, visible_row: usize) -> Option<usize> {
    let line_index = usize::from(scroll).checked_add(visible_row)?;
    let line = content.lines().nth(line_index)?;
    let reference = line.strip_prefix("[LINK ")?;
    let (number, _) = reference.split_once(']')?;
    number.parse::<usize>().ok()?.checked_sub(1)
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

async fn open_url(backend: &StdBackend, state: &mut AppState, url: &str) -> Option<String> {
    let page = match backend.fetch_page(url).await {
        Ok(page) => page,
        Err(error) => return Some(error.to_string()),
    };
    match HtmlParser::parse_article_at(&page.body, page.url.as_str()) {
        Ok(article) => {
            state.open_article(
                article.title,
                article.content,
                article.links,
                article.images,
                article.forms,
            );
            state.input.clear();
            state.bookmark_input.clear();
            None
        }
        Err(error) => Some(error.to_string()),
    }
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
        .fetch_bytes(&image.url)
        .await
        .map_err(|error| format!("Image download failed: {error}"))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image download exceeded the 20 MiB limit.".to_owned());
    }
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| format!("Could not identify image format: {error}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_IMAGE_PIXELS {
        return Err("Image is too large to display safely.".to_owned());
    }
    if width == 0 || height == 0 {
        return Err("Image has invalid dimensions.".to_owned());
    }
    let image = image::load_from_memory(&bytes)
        .map_err(|error| format!("Could not decode image: {error}"))?;
    Ok(ImageViewer {
        index,
        image: image.to_rgba8(),
    })
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
        KeyCode::Char(character @ '1'..='9') => {
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
    selected_link: usize,
    show_help: bool,
    notice: Option<&str>,
    modal: Option<ActiveModal<'_>>,
) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.size());

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
        Span::raw("  [Tor: DISCONNECTED]"),
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
            content,
            links,
            images,
            forms,
        } => {
            let lines = content
                .lines()
                .map(|line| {
                    let style = if link_reference_index(line) == Some(selected_link) {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    Line::from(Span::styled(line.to_owned(), style))
                })
                .collect::<Vec<_>>();
            let focused_link = links.get(selected_link).map_or_else(
                || "[f] Fill form".to_owned(),
                |url| {
                    format!(
                        "Link {} of {}: {url}  [Tab] Next  [Shift+Tab] Previous  [Enter] Open  [f] Form",
                        selected_link + 1,
                        links.len()
                    )
                },
            );
            let focused_link = format!("{focused_link}  [i] View image ({})", images.len());
            let article = Paragraph::new(lines)
                .block(
                    Block::default()
                        .title(format!("{title}  [f] Fill form ({})", forms.len()))
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: true })
                .scroll((state.scroll, 0));
            frame.render_widget(article, areas[1]);
            frame.render_widget(Paragraph::new(focused_link), areas[2]);
        }
    }

    if !matches!(state.view, AppView::ArticleReader { .. }) {
        frame.render_widget(
            Paragraph::new(
                "[Ctrl+L] Search  [F1] Help  [Tab] Bookmark/Next Link  [1-9] Select  [PgUp/PgDn] Page  [Esc] Back  [q] Quit",
            ),
            areas[2],
        );
    }
    let status = format_metrics(Some(metrics));
    let status = if let Some(message) = notice {
        format!("{message}  |  {status}")
    } else {
        status
    };
    frame.render_widget(Paragraph::new(status), areas[3]);
    if show_help {
        let area = centered_rect(60, 40, frame.size());
        frame.render_widget(
            Paragraph::new(
                "Keyboard: Ctrl+L returns to search; Tab/Shift+Tab moves between links/fields; i opens images (Left/Right changes images while open); f opens forms; arrows scroll/select; Enter submits; Esc returns; q/Ctrl+C quits.\nMouse: click a result or [LINK N] reference to select it; use the wheel to scroll.",
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
                    render_image_viewer(frame, viewer, image, images.len());
                }
            }
        }
        None => {}
    }
}

fn render_image_viewer(
    frame: &mut ratatui::Frame<'_>,
    viewer: &ImageViewer,
    reference: &ArticleImage,
    image_count: usize,
) {
    let area = centered_rect(90, 85, frame.size());
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
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().fg(Color::White).bg(Color::Black));
    frame.render_widget(Clear, area);
    frame.render_widget(block.clone(), area);
    let inner = block.inner(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let source_width = f64::from(viewer.image.width());
    let source_height = f64::from(viewer.image.height());
    let scale =
        (f64::from(inner.width) / source_width).min(f64::from(inner.height) / source_height);
    if !scale.is_finite() || scale <= 0.0 {
        return;
    }
    let target_width = (source_width * scale)
        .floor()
        .max(1.0)
        .min(f64::from(inner.width)) as u32;
    let target_height = (source_height * scale * 2.0)
        .floor()
        .max(1.0)
        .min(f64::from(inner.height.saturating_mul(2))) as u32;
    let resized = image::imageops::resize(
        &viewer.image,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    let offset_x = (inner.width - target_width as u16) / 2;
    let offset_y = (inner.height.saturating_mul(2) - target_height as u16) / 2;
    let lines = (0..inner.height)
        .map(|row| {
            let spans = (0..inner.width)
                .map(|column| {
                    let x = column.saturating_sub(offset_x);
                    let top_position = row.saturating_mul(2);
                    let bottom_position = top_position.saturating_add(1);
                    let top_y = top_position.saturating_sub(offset_y);
                    let bottom_y = bottom_position.saturating_sub(offset_y);
                    let top = if column >= offset_x
                        && x < target_width as u16
                        && top_position >= offset_y
                        && top_y < target_height as u16
                    {
                        pixel_color(resized.get_pixel(u32::from(x), u32::from(top_y)).0)
                    } else {
                        Color::Black
                    };
                    let bottom = if column >= offset_x
                        && x < target_width as u16
                        && bottom_position >= offset_y
                        && bottom_y < target_height as u16
                    {
                        pixel_color(resized.get_pixel(u32::from(x), u32::from(bottom_y)).0)
                    } else {
                        Color::Black
                    };
                    Span::styled("▀", Style::default().fg(top).bg(bottom))
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

fn link_reference_index(line: &str) -> Option<usize> {
    let reference = line.strip_prefix("[LINK ")?;
    let (number, _) = reference.split_once(']')?;
    number.parse::<usize>().ok()?.checked_sub(1)
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
        cycle_link, handle_key, link_reference_index, next_form_field, search_searxng,
        validate_form, FormSession,
    };
    use crossterm::event::KeyCode;
    use nyx_core::{AppState, FormField, FormFieldKind, FormMethod, HtmlForm};
    use nyx_pal::{AdapterError, AdapterFuture, FormSubmission, NetworkAdapter, PageResponse};
    use std::sync::atomic::{AtomicBool, Ordering};

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
            "[LINK 1] https://one.test\n[LINK 2] https://two.test".to_owned(),
            vec!["https://one.test".to_owned(), "https://two.test".to_owned()],
            Vec::new(),
            Vec::new(),
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
    fn parses_link_reference_index() {
        assert_eq!(link_reference_index("[LINK 3] https://three.test"), Some(2));
        assert_eq!(link_reference_index("article content"), None);
    }
}
