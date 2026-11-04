use crate::{ArticleImage, HtmlForm, ReaderDocument};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppView {
    Home,
    SearchResults {
        query: String,
        links: Vec<SearchResult>,
    },
    ArticleReader {
        title: String,
        content: String,
        links: Vec<String>,
        images: Vec<ArticleImage>,
        forms: Vec<HtmlForm>,
        reader: ReaderDocument,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub view: AppView,
    pub input: String,
    pub bookmark_input: String,
    pub scroll: u16,
    pub result_page: usize,
    previous_views: Vec<AppView>,
}

pub const RESULTS_PER_PAGE: usize = 10;

impl Default for AppState {
    fn default() -> Self {
        Self {
            view: AppView::Home,
            input: String::new(),
            bookmark_input: String::new(),
            scroll: 0,
            result_page: 0,
            previous_views: Vec::new(),
        }
    }
}

impl AppState {
    pub fn submit_search(&mut self) {
        let query = self.input.trim().to_owned();
        if query.is_empty() {
            return;
        }

        self.push_view(AppView::SearchResults {
            query,
            links: Vec::new(),
        });
        self.input.clear();
    }

    pub fn update_search_results(&mut self, query: &str, links: Vec<SearchResult>) -> bool {
        if let AppView::SearchResults {
            query: current_query,
            links: current_links,
        } = &mut self.view
        {
            if current_query == query {
                *current_links = links;
                self.result_page = 0;
                self.scroll = 0;
                return true;
            }
        }
        false
    }

    pub fn open_article(
        &mut self,
        title: String,
        content: String,
        links: Vec<String>,
        images: Vec<ArticleImage>,
        forms: Vec<HtmlForm>,
        reader: ReaderDocument,
    ) {
        self.push_view(AppView::ArticleReader {
            title,
            content,
            links,
            images,
            forms,
            reader,
        });
    }

    pub fn go_back(&mut self) {
        if let Some(view) = self.previous_views.pop() {
            self.view = view;
            self.scroll = 0;
            self.result_page = 0;
        } else {
            self.view = AppView::Home;
            self.scroll = 0;
            self.result_page = 0;
        }
    }

    pub fn go_home(&mut self) {
        self.view = AppView::Home;
        self.input.clear();
        self.scroll = 0;
        self.result_page = 0;
        self.previous_views.clear();
    }

    fn push_view(&mut self, view: AppView) {
        self.previous_views.push(self.view.clone());
        self.view = view;
        self.scroll = 0;
        self.result_page = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, AppView};
    use crate::ReaderDocument;

    #[test]
    fn search_submission_and_back_navigation_preserve_view_history() {
        let mut state = AppState {
            input: "  privacy  ".to_owned(),
            ..AppState::default()
        };
        state.submit_search();

        assert_eq!(
            state.view,
            AppView::SearchResults {
                query: "privacy".to_owned(),
                links: Vec::new()
            }
        );
        assert!(state.input.is_empty());

        state.go_back();
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn empty_search_does_not_change_view() {
        let mut state = AppState::default();
        state.submit_search();
        assert_eq!(state.view, AppView::Home);
    }

    #[test]
    fn go_home_clears_navigation_history_and_resets_search_input() {
        let mut state = AppState {
            input: "old query".to_owned(),
            ..AppState::default()
        };
        state.submit_search();
        state.open_article(
            "Article".to_owned(),
            "Text".to_owned(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            ReaderDocument {
                title: "Article".to_owned(),
                byline: None,
                blocks: Vec::new(),
                links: Vec::new(),
            },
        );
        state.go_home();
        state.go_back();

        assert_eq!(state.view, AppView::Home);
        assert!(state.input.is_empty());
        assert_eq!(state.scroll, 0);
        assert_eq!(state.result_page, 0);
    }
}
