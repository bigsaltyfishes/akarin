use alloc::string::{String, ToString};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    BadFormat,
    UnsafeChar { character: char, position: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Url {
    inner: String,
}

impl Url {
    /// Creates a new `Url` from the given protocol and path.
    ///
    /// The `proto` parameter should be in the format `protocol:path`, where
    /// `protocol` is a string that identifies the protocol (e.g., "file",
    /// "http") and `path` is the path to the resource. The method checks for
    /// unsafe characters in the input and returns an error if any are found.
    /// Unsafe characters include control characters, whitespace, and certain
    /// special characters that could cause issues in URLs.
    pub fn new<S, S2>(proto: S, ident: S2) -> Result<Self, UrlError>
    where
        S: AsRef<str>,
        S2: AsRef<str>,
    {
        let s = proto.as_ref();
        let ident = ident.as_ref();
        if let Some(c) = Self::find_unsafe_char(s, Some(":/")) {
            return Err(c);
        }

        if let Some(c) = Self::find_unsafe_char(ident, None) {
            return Err(c);
        }

        Ok(Url {
            inner: format!("{}://{}/", s, ident),
        })
    }

    /// Creates a new `Url` from the given string.
    ///
    /// The `url_str` parameter should be in the format `protocol:path`, where
    /// `protocol` is a string that identifies the protocol (e.g., "file",
    /// "http") and `path` is the path to the resource. The method checks for
    /// unsafe characters in the input and returns an error if any are found.
    /// Unsafe characters include control characters, whitespace, and certain
    /// special characters that could cause issues in URLs.
    pub fn from_str<S>(url_str: S) -> Result<Self, UrlError>
    where
        S: AsRef<str>,
    {
        let url_str = url_str.as_ref();

        // Scheme Layout
        // <protocol>://<identifier>/<path>
        let mut components = url_str.split("://");
        let protocol = components.next().ok_or(UrlError::BadFormat)?;
        let (ident, path) = {
            let part = components.next().ok_or(UrlError::BadFormat)?;
            part.split_once('/').ok_or(UrlError::BadFormat)?
        };

        if let Some(c) = Self::find_unsafe_char(protocol, Some(":/")) {
            return Err(c);
        }

        if let Some(c) = Self::find_unsafe_char(ident, None) {
            return Err(c);
        }

        if let Some(c) = Self::find_unsafe_char(path, None) {
            return Err(c);
        }

        Ok(Url {
            inner: url_str.to_string(),
        })
    }

    /// Returns the protocol part of the URL.
    ///
    /// The protocol is the substring before the first colon (`:`) in the URL.
    /// If the URL does not contain a colon, this method will panic with a
    /// message indicating that the inner data is corrupted, as it expects a
    /// valid URL format.
    pub fn proto(&self) -> &str {
        self.inner
            .split("://")
            .next()
            .expect("Corrupted inner data")
    }

    /// Returns the identifier part of the URL.
    ///
    /// The identifier is the substring between the protocol and the path in the
    /// URL. If the URL does not contain a colon or a slash, this method
    /// will panic with a message indicating that the inner data is
    /// corrupted, as it expects a valid URL format.
    pub fn identifier(&self) -> &str {
        self.inner
            .split("://")
            .nth(1)
            .expect("Corrupted inner data")
            .split_once('/')
            .map(|(ident, _)| ident)
            .expect("Corrupted inner data")
    }

    /// Returns the path part of the URL.
    ///
    /// The path is the substring after the first colon (`:`) in the URL. If
    /// the URL does not contain a colon, this method will panic with a message
    /// indicating that the inner data is corrupted, as it expects a valid URL
    /// format.
    pub fn path(&self) -> &str {
        self.inner
            .split("://")
            .nth(1)
            .expect("Corrupted inner data")
            .split_once('/')
            .map(|(_, path)| path)
            .expect("Corrupted inner data")
    }

    /// Returns an iterator over the components of the path.
    ///
    /// The path is split by the slash (`/`) character, and empty components are
    /// filtered out. This allows you to iterate over the individual segments of
    /// the path in the URL. For example, if the path is `/foo/bar/baz`,
    /// this method will return an iterator that yields `foo`, `bar`, and `baz`.
    pub fn path_components(&self) -> impl Iterator<Item = &str> {
        self.path()
            .split('/')
            .filter(|component| !component.is_empty())
    }

    fn find_unsafe_char(input: &str, extra: Option<&str>) -> Option<UrlError> {
        input
            .chars()
            .enumerate()
            .find(|&(_, c)| {
                c.is_control()
                    || c.is_whitespace()
                    || "<>\"`{}|\\^".contains(c)
                    || extra.map_or(false, |e| e.contains(c))
            })
            .map(|(i, c)| UrlError::UnsafeChar {
                character: c,
                position: i,
            })
    }
}
