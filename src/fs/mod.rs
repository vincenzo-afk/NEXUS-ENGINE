//! Filesystem access layer: recursive crawling and file filtering.

mod crawler;

pub use crawler::{crawl_folder, CrawlOptions, CrawledFile};
