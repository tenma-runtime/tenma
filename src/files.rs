//! Static files, without PHP: serve($port, $handler, static: ['/assets' => $directory]). If the
//! path is under a prefix and there is a file there, Rust serves it, otherwise the request goes to
//! the handler. Paths outside all prefixes go to PHP right away, without looking at the file system.
//!
//! Safe: the file is opened and only then checked for where it really is (via the file descriptor,
//! so no race with swapped symlinks): inside the directory and with no part starting with a dot
//! (.., .env, .git), not even through a symlink to there. It is served from exactly this descriptor.
//! Plus Content-Type by extension, ETag and Last-Modified with 304, a single range (206) with If-Range
//! and streaming in chunks instead of in memory.

use std::ffi::OsStr;
use std::fs::File;
use std::io::SeekFrom;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use bytes::{Bytes, BytesMut};
use http_body_util::{Either, Full};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, Response, StatusCode};
use tokio::sync::mpsc;

use crate::ResponseBody;
use crate::streams::ChannelBody;

const CHUNK: usize = 64 * 1024;

/// A prefix with a directory or a single file.
pub struct Mount {
    /// Without a trailing /; "/" matches everything.
    pub prefix: String,
    /// The directory; for a single file, the one it is in.
    pub root: PathBuf,
    /// For a single file, its name, encoded as in a URL.
    pub file: Option<String>,
}

impl Mount {
    /// A prefix and what is served there; None if it is neither a directory nor a file.
    pub fn new(prefix: String, target: &Path) -> Option<Self> {
        if target.is_dir() {
            return Some(Self {
                prefix,
                root: target.to_path_buf(),
                file: None,
            });
        }
        let name = target.file_name().filter(|_| target.is_file())?;
        let file =
            percent_encoding::percent_encode(name.as_bytes(), percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        Some(Self {
            prefix,
            root: target.parent()?.to_path_buf(),
            file: Some(file),
        })
    }

    /// The path below the directory, if $path falls under this prefix.
    fn rest<'a>(&'a self, path: &'a str) -> Option<&'a str> {
        let rest = if self.prefix == "/" {
            path
        } else {
            path.strip_prefix(self.prefix.as_str())?
        };
        match &self.file {
            // A single file only matches its exact prefix
            Some(file) => rest.is_empty().then_some(file.as_str()),
            None => (rest.is_empty() || rest.starts_with('/')).then_some(rest),
        }
    }
}

enum Found {
    File(File, std::fs::Metadata, PathBuf),
    /// A directory without a trailing /: redirect so relative links in it work.
    Directory,
}

/// The real path behind an open descriptor.
#[cfg(target_os = "macos")]
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes including the null byte into the buffer
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } == -1 {
        return None;
    }
    let length = buffer.iter().position(|&byte| byte == 0)?;
    buffer.truncate(length);
    Some(OsStr::from_bytes(&buffer).into())
}

#[cfg(not(target_os = "macos"))]
fn fd_path(file: &File) -> Option<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

/// Extensions PHP may execute: their source is never handed out, so the front controller can sit in the directory.
const SOURCE_EXTENSIONS: [&str; 7] = ["php", "phtml", "phar", "phps", "pht", "php5", "php7"];

/// What tenma itself reads or writes: the script, its tenma.json, the php.ini of -c, the TLS certificate and
/// key, the --record directory. A static root that holds them (`static: __DIR__`) never hands them out.
static PROTECTED: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Keeps $paths, files or directories, out of every static root. Once, before the servers start.
pub fn protect(paths: Vec<PathBuf>) {
    let resolved = paths
        .into_iter()
        .map(|path| {
            std::fs::canonicalize(&path).unwrap_or_else(|_| {
                // Not there yet (the --record directory is created later): resolve at least the parent
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
                let name = path.file_name().map(PathBuf::from).unwrap_or_default();
                std::fs::canonicalize(&parent).unwrap_or(parent).join(name)
            })
        })
        .collect();
    let _ = PROTECTED.set(resolved);
}

/// Whether $real, the opened file's path, is one of tenma's own files or lies in one of its directories.
fn protected(real: &Path) -> bool {
    real.file_name().is_some_and(|name| name == "tenma.json")
        || PROTECTED
            .get()
            .is_some_and(|paths| paths.iter().any(|path| real.starts_with(path)))
}

/// Opens $candidate and checks that the opened file really is under $root, is not hidden, is no PHP source
/// and is none of tenma's own files.
fn open_inside(root: &Path, candidate: &Path) -> Option<(File, std::fs::Metadata, PathBuf)> {
    // O_NONBLOCK: a named pipe in the directory would otherwise make open() wait forever; files do not mind
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(candidate)
        .ok()?;
    let real = fd_path(&file)?;
    let inside = real.strip_prefix(root).ok()?;
    let hidden = inside
        .components()
        .any(|part| !matches!(part, Component::Normal(name) if !name.as_bytes().starts_with(b".")));
    let source = real
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            SOURCE_EXTENSIONS
                .iter()
                .any(|php| ext.eq_ignore_ascii_case(php))
        });
    let meta = file.metadata().ok()?;
    (!hidden && !source && !protected(&real)).then_some((file, meta, real))
}

/// The file for the path under $root, if there is one that may be served. Blocks: runs on a
/// background thread.
fn resolve(root: &Path, path: &str) -> Option<Found> {
    // Resolved per request: if the directory points elsewhere after a deploy (symlink swap), the new one applies
    let root = std::fs::canonicalize(root).ok()?;
    let decoded: Vec<u8> = percent_encoding::percent_decode_str(path).collect();
    let mut candidate = root.clone();
    for segment in decoded
        .split(|&byte| byte == b'/')
        .filter(|segment| !segment.is_empty())
    {
        if segment.starts_with(b".") || segment.contains(&0) || segment.contains(&b'\\') {
            return None;
        }
        candidate.push(OsStr::from_bytes(segment));
    }
    let (file, meta, real) = open_inside(&root, &candidate)?;
    if meta.is_file() {
        return Some(Found::File(file, meta, real));
    }
    if !meta.is_dir() {
        return None;
    }
    if !path.ends_with('/') {
        return Some(Found::Directory);
    }
    let (file, meta, real) = open_inside(&root, &real.join("index.html"))?;
    meta.is_file().then_some(Found::File(file, meta, real))
}

/// A single range from "bytes=a-b", "bytes=a-" or "bytes=-n". None: none requested or an invalid
/// one, ignored as in RFC 9110 (whole file). Some(Err): none fits (416).
fn range(value: &str, size: u64) -> Option<Result<(u64, u64), ()>> {
    let value = value.strip_prefix("bytes=")?;
    // Multiple ranges would be multipart/byteranges: send the whole file instead
    let (start, end) = value.split_once('-').filter(|_| !value.contains(','))?;
    let number = |text: &str| {
        text.trim()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| text.trim().parse::<u64>().ok())
            .flatten()
    };
    if size == 0 {
        return None;
    }
    let last = size - 1;
    Some(match (start.trim().is_empty(), end.trim().is_empty()) {
        (true, false) => {
            let count = number(end).filter(|&count| count > 0)?;
            Ok((size.saturating_sub(count), last))
        }
        (false, true) => {
            let start = number(start)?;
            if start > last {
                Err(())
            } else {
                Ok((start, last))
            }
        }
        (false, false) => {
            let (start, end) = (number(start)?, number(end)?);
            if start > end {
                return None;
            }
            if start > last {
                Err(())
            } else {
                Ok((start, end.min(last)))
            }
        }
        (true, true) => return None,
    })
}

fn empty(status: StatusCode) -> Response<ResponseBody> {
    let mut response = Response::new(Either::Left(Full::new(Bytes::new())));
    *response.status_mut() = status;
    response
}

fn text(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

/// Serves the file if there is one for the request; None if the handler should take over.
/// $mounts are sorted by prefix length, longest first: it wins.
pub async fn serve(
    mounts: &[Mount],
    method: &Method,
    uri: &hyper::Uri,
    headers: &HeaderMap,
) -> Option<Response<ResponseBody>> {
    if method != Method::GET && method != Method::HEAD {
        return None;
    }
    let (mount, rest) = mounts
        .iter()
        .find_map(|mount| Some((mount, mount.rest(uri.path())?)))?;
    // A directory prefix without / ("/assets") counts as a directory without a trailing /: redirect
    let (root, rest) = (
        mount.root.clone(),
        if mount.file.is_some() {
            format!("/{rest}")
        } else {
            rest.to_owned()
        },
    );
    let found = tokio::task::spawn_blocking(move || resolve(&root, &rest))
        .await
        .ok()??;
    let (file, meta, real) = match found {
        Found::File(file, meta, real) => (file, meta, real),
        Found::Directory => {
            let mut response = empty(StatusCode::MOVED_PERMANENTLY);
            // One leading slash: "//docs/" would send the browser to the host "docs"
            let path = uri.path().trim_start_matches('/');
            let location = match uri.query() {
                Some(query) => format!("/{path}/?{query}"),
                None => format!("/{path}/"),
            };
            response
                .headers_mut()
                .insert(header::LOCATION, HeaderValue::from_str(&location).ok()?);
            return Some(response);
        }
    };
    let size = meta.len();
    let modified = meta.modified().unwrap_or(UNIX_EPOCH);
    let since_epoch = modified.duration_since(UNIX_EPOCH).unwrap_or_default();
    let etag = format!("\"{:x}-{:x}\"", size, since_epoch.as_nanos());
    let last_modified = httpdate::fmt_http_date(modified);

    // If-None-Match takes precedence; If-Modified-Since only without it (RFC 9110 13.2.2)
    let not_modified = match (
        text(headers, header::IF_NONE_MATCH),
        text(headers, header::IF_MODIFIED_SINCE),
    ) {
        (Some(tags), _) => tags
            .split(',')
            .any(|tag| tag.trim() == "*" || tag.trim().trim_start_matches("W/") == etag),
        (None, Some(since)) => httpdate::parse_http_date(since).is_ok_and(|since| {
            since
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                >= since_epoch.as_secs()
        }),
        (None, None) => false,
    };

    let mime = mime_guess::from_path(&real).first_or_octet_stream();
    let content_type = if mime.type_() == mime_guess::mime::TEXT
        || matches!(
            mime.essence_str(),
            "application/javascript" | "application/json"
        ) {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    };

    let mut response = if not_modified {
        empty(StatusCode::NOT_MODIFIED)
    } else {
        // Range only if If-Range is missing or still matches this version of the file; otherwise the whole file
        let current = text(headers, header::IF_RANGE)
            .is_none_or(|validator| validator == etag || validator == last_modified);
        let requested = text(headers, header::RANGE)
            .filter(|_| current)
            .and_then(|value| range(value, size));
        let (status, start, length) = match requested {
            Some(Ok((start, end))) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
            Some(Err(())) => {
                let mut response = empty(StatusCode::RANGE_NOT_SATISFIABLE);
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}")).ok()?,
                );
                return Some(response);
            }
            None => (StatusCode::OK, 0, size),
        };
        let mut response = if method == Method::HEAD {
            empty(status)
        } else {
            Response::new(Either::Right(stream(file, start, length)))
        };
        *response.status_mut() = status;
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
        if status == StatusCode::PARTIAL_CONTENT {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{}/{size}", start + length - 1))
                    .ok()?,
            );
        }
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type).ok()?,
        );
        response
    };
    let headers = response.headers_mut();
    headers.insert(header::ETAG, HeaderValue::from_str(&etag).ok()?);
    headers.insert(
        header::LAST_MODIFIED,
        HeaderValue::from_str(&last_modified).ok()?,
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // The type stays the one the extension says: an upload can't pass itself off as a script
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // An SVG can carry scripts; opened directly (an upload under a mount) it runs none on this origin.
    // As <img> it is unaffected. HTML files stay as they are: a static site needs their scripts
    if mime.essence_str() == "image/svg+xml" {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
    Some(response)
}

/// Reads $length bytes from $start out of the already opened file, chunk by chunk; if the client
/// cannot keep up, reading waits without holding a blocking thread; if it is gone, reading ends.
fn stream(file: File, start: u64, length: u64) -> ChannelBody {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut file = tokio::fs::File::from_std(file);
        if file.seek(SeekFrom::Start(start)).await.is_err() {
            return;
        }
        let mut reader = file.take(length);
        let mut buffer = BytesMut::new();
        loop {
            buffer.resize(CHUNK, 0);
            match reader.read(&mut buffer).await {
                // Shorter than announced (file truncated meanwhile): hyper aborts the connection
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if sender.send(buffer.split_to(read).freeze()).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    ChannelBody::sized(receiver)
}
