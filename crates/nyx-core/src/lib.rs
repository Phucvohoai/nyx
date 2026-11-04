mod parser;
mod search;
mod state;

pub use parser::{
    Article, ArticleImage, FormField, FormFieldKind, FormMethod, HtmlForm, HtmlParser, ParserError,
    ReaderBlock, ReaderDocument, ReaderInline, ReaderLinkItem,
};
pub use search::{
    default_searxng_endpoint, duckduckgo_search_endpoint, duckduckgo_search_url,
    normalize_direct_url, parse_duckduckgo_results, parse_searxng_results, rank_results,
    searxng_search_url, SearchError,
};
pub use state::{AppState, AppView, SearchResult, RESULTS_PER_PAGE};
