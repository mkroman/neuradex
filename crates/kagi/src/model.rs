//! The search result models returned by the Kagi client.

/// Represents a single search result obtained from the search operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// The title of the search result.
    pub title: String,
    /// The URL of the search result.
    pub url: String,
    /// The description.
    pub description: String,
    /// The 1-based rank reported by Kagi's structured results payload.
    ///
    /// Zero when the result was parsed from the HTML fallback, which does not carry a rank.
    pub rank: u32,
    /// The source domain reported by Kagi's structured results payload.
    ///
    /// Empty when the result was parsed from the HTML fallback, which does not carry a source.
    pub source: String,
}

/// Represents a single image result obtained from the image search operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageResult {
    /// The title of the image.
    pub title: String,
    /// The URL of the page the image was found on.
    pub page_url: String,
    /// The URL of the original full-size image.
    ///
    /// Kagi reports the sentinel `#no_preview` for items it cannot preview; consumers should
    /// treat that value as "no image URL" rather than fetching it.
    pub image_url: String,
    /// The URL of the thumbnail served through Kagi's image proxy.
    pub thumbnail_url: String,
    /// The width of the image in pixels.
    pub width: u32,
    /// The height of the image in pixels.
    pub height: u32,
    /// The hostname of the page the image was found on.
    pub host: String,
    /// Kagi's rank field for the image.
    ///
    /// Kagi currently reports `0` for every image result, so this is not an ordinal.
    pub rank: u32,
}
