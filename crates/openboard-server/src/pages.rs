//! The web UI's static files, embedded in the binary.
//!
//! The files live in this crate's `web/` directory and are compiled in, so
//! the server reads nothing from disk to serve them and makes no request to
//! any other origin. They hold no board content, which is why the routes
//! that serve them need no credential; see `openboard_api::web`.
//!
//! To add a file, put it in `web/assets/` and add a line to [`ASSETS`].

/// Path of the page.
pub(crate) const INDEX_PATH: &str = "/";
/// Path of the sign-in page, which is the same page.
pub(crate) const JOIN_PATH: &str = openboard_api::web::JOIN_PAGE_PATH;
/// Every asset is served directly under this prefix.
pub(crate) const ASSETS_PREFIX: &str = "/assets/";

pub(crate) const INDEX_HTML: &str = include_str!("../web/index.html");
pub(crate) const INDEX_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// One file served at `/assets/<name>`.
pub(crate) struct Asset {
    /// The file name in the URL. No slashes.
    pub name: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

/// Every asset the server has. A name that is not here is a `404`.
pub(crate) const ASSETS: &[Asset] = &[
    Asset {
        name: "app.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("../web/assets/app.js"),
    },
    Asset {
        name: "app.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("../web/assets/app.css"),
    },
];

pub(crate) fn find_asset(name: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_are_plain_and_unique() {
        for (index, asset) in ASSETS.iter().enumerate() {
            assert!(!asset.name.is_empty() && !asset.name.contains('/'));
            assert!(!asset.content_type.is_empty());
            assert!(ASSETS[..index].iter().all(|other| other.name != asset.name));
            assert!(find_asset(asset.name).is_some());
        }
        assert!(find_asset("index.html").is_none());
        assert!(find_asset("../index.html").is_none());
        assert!(find_asset("").is_none());
    }

    #[test]
    fn the_page_loads_only_its_own_assets() {
        // The content security policy would refuse anything else anyway.
        assert!(INDEX_HTML.contains("/assets/app.js"));
        assert!(INDEX_HTML.contains("/assets/app.css"));
        for foreign in ["http://", "https://", "src=\"//", "href=\"//"] {
            assert!(!INDEX_HTML.contains(foreign), "{foreign}");
        }
    }
}
