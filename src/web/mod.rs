//! Web crawling: HTTP fetching, robots.txt and sitemap handling, URL
//! canonicalization, the crawl queue/scheduler, and the crawler that ties
//! them all together with HTML content extraction and indexing.

pub mod canonical;
pub mod crawler;
pub mod feed;
pub mod http;
pub mod queue;
pub mod robots;
pub mod sitemap;

pub use crawler::{CrawlOptions as WebCrawlOptions, WebCrawler};
