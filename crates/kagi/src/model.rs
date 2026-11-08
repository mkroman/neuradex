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
}

/// Represents a single image result obtained from the image search operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageResult {
    /// The title of the image.
    pub title: String,
    /// The URL of the page the image was found on.
    pub page_url: String,
    /// The URL of the full-size image.
    pub image_url: String,
    /// The URL of the thumbnail served through Kagi's image proxy.
    pub thumbnail_url: String,
    /// The width of the image in pixels.
    pub width: u32,
    /// The height of the image in pixels.
    pub height: u32,
    /// The hostname of the page the image was found on.
    pub host: String,
    /// The rank of the image within the results.
    pub rank: u32,
}
