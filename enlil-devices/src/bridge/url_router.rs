//! URL / protocol-handler scheme routing (Enlil Bridge 3.7.6).
//!
//! When a guest opens a URL (a web link, a `mailto:`, a custom app scheme), the
//! bridge routes it to whichever guest is registered as the handler for that
//! URL's scheme — so, e.g., every `mailto:` opens in the guest that owns the
//! mail client while `https:` opens in the browser guest. This module owns the
//! scheme→guest table and the resolution logic; delivering the resolved URL to
//! the target guest is the transport layer's job.

use std::collections::HashMap;

/// The outcome of resolving a URL against the routing table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlResolution {
    /// Route the URL to this guest (a scheme handler, or the default).
    RouteToGuest(String),
    /// The URL parsed to a scheme with no registered handler and no default.
    NoHandler(String),
    /// The string is not a valid URL (no parseable scheme).
    InvalidUrl,
}

/// Parse and normalize the scheme of a URL.
///
/// Per RFC 3986: `scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )`,
/// terminated by `:`. Returns the lowercased scheme (schemes are
/// case-insensitive), or `None` when the string has no `:` or the leading token
/// is not a valid scheme.
#[must_use]
pub fn parse_scheme(url: &str) -> Option<String> {
    let colon = url.find(':')?;
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        Some(scheme.to_ascii_lowercase())
    } else {
        None
    }
}

/// Normalize a scheme token for storage/lookup: trim, drop a trailing `:`, and
/// lowercase, so `"HTTP"`, `"http"`, and `"http:"` register the same handler.
fn normalize_scheme(scheme: &str) -> String {
    scheme.trim().trim_end_matches(':').to_ascii_lowercase()
}

/// Scheme → guest routing table for URL / protocol-handler forwarding.
#[derive(Debug, Clone, Default)]
pub struct UrlRouter {
    /// Registered scheme handlers (scheme, lowercased) → target guest.
    handlers: HashMap<String, String>,
    /// Guest for a valid URL whose scheme has no explicit handler.
    default_guest: Option<String>,
}

impl UrlRouter {
    /// A new empty router with no handlers and no default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `guest` as the handler for `scheme` (normalized), replacing any
    /// previous handler for that scheme.
    pub fn register(&mut self, scheme: &str, guest: &str) {
        self.handlers
            .insert(normalize_scheme(scheme), guest.to_string());
    }

    /// Remove the handler for `scheme`, returning the guest it pointed at.
    pub fn unregister(&mut self, scheme: &str) -> Option<String> {
        self.handlers.remove(&normalize_scheme(scheme))
    }

    /// Set (or clear) the fallback guest for schemes with no explicit handler.
    pub fn set_default_guest(&mut self, guest: Option<String>) {
        self.default_guest = guest;
    }

    /// The current fallback guest, if any.
    #[must_use]
    pub fn default_guest(&self) -> Option<&str> {
        self.default_guest.as_deref()
    }

    /// The guest registered for `scheme` (normalized), if any.
    #[must_use]
    pub fn handler_for(&self, scheme: &str) -> Option<&str> {
        self.handlers
            .get(&normalize_scheme(scheme))
            .map(String::as_str)
    }

    /// Resolve a URL to a target guest: parse its scheme, prefer the scheme's
    /// registered handler, else the default guest, else report no handler; a URL
    /// with no valid scheme is [`UrlResolution::InvalidUrl`].
    #[must_use]
    pub fn resolve(&self, url: &str) -> UrlResolution {
        let Some(scheme) = parse_scheme(url) else {
            return UrlResolution::InvalidUrl;
        };
        if let Some(guest) = self.handlers.get(&scheme) {
            return UrlResolution::RouteToGuest(guest.clone());
        }
        self.default_guest.as_ref().map_or_else(
            || UrlResolution::NoHandler(scheme.clone()),
            |guest| UrlResolution::RouteToGuest(guest.clone()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_schemes_case_insensitively() {
        assert_eq!(parse_scheme("https://example.com"), Some("https".into()));
        assert_eq!(parse_scheme("HTTP://EXAMPLE"), Some("http".into()));
        assert_eq!(parse_scheme("mailto:a@b.com"), Some("mailto".into()));
        assert_eq!(parse_scheme("tel:+1-555"), Some("tel".into()));
        // "+", "-", "." are legal scheme characters.
        assert_eq!(parse_scheme("web+enlil:x"), Some("web+enlil".into()));
    }

    #[test]
    fn rejects_strings_without_a_valid_scheme() {
        assert_eq!(parse_scheme("no-colon-here"), None);
        assert_eq!(parse_scheme(":leading-colon"), None);
        assert_eq!(parse_scheme("1http://x"), None); // must start with a letter
        assert_eq!(parse_scheme("ht tp://x"), None); // space is illegal
    }

    #[test]
    fn resolves_to_the_registered_scheme_handler() {
        let mut router = UrlRouter::new();
        router.register("https", "browser");
        router.register("mailto:", "mail"); // trailing colon normalized away
        assert_eq!(
            router.resolve("https://example.com"),
            UrlResolution::RouteToGuest("browser".into())
        );
        // Scheme match is case-insensitive.
        assert_eq!(
            router.resolve("MAILTO:a@b.com"),
            UrlResolution::RouteToGuest("mail".into())
        );
        assert_eq!(router.handler_for("HTTPS"), Some("browser"));
    }

    #[test]
    fn falls_back_to_the_default_guest() {
        let mut router = UrlRouter::new();
        router.register("https", "browser");
        router.set_default_guest(Some("linux1".into()));
        // Unregistered scheme routes to the default.
        assert_eq!(
            router.resolve("ftp://server/file"),
            UrlResolution::RouteToGuest("linux1".into())
        );
        assert_eq!(router.default_guest(), Some("linux1"));
    }

    #[test]
    fn reports_no_handler_when_no_default() {
        let router = UrlRouter::new();
        assert_eq!(
            router.resolve("ftp://server"),
            UrlResolution::NoHandler("ftp".into())
        );
    }

    #[test]
    fn reports_invalid_url_for_a_schemeless_string() {
        let router = UrlRouter::new();
        assert_eq!(router.resolve("just some text"), UrlResolution::InvalidUrl);
    }

    #[test]
    fn unregister_removes_a_handler() {
        let mut router = UrlRouter::new();
        router.register("tel", "phone");
        assert_eq!(router.unregister("tel"), Some("phone".into()));
        assert_eq!(
            router.resolve("tel:+1"),
            UrlResolution::NoHandler("tel".into())
        );
        assert_eq!(router.unregister("tel"), None);
    }
}
