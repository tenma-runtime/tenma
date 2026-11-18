//! Async file system for PHP (FileSystem\FileInfo and DirectoryInfo). The work runs on tokio's background threads, the fiber
//! waits without blocking the worker. Paths stay bytes as in PHP (including invalid UTF-8); the
//! C layer has already resolved them against PHP's working directory and checked open_basedir.
//!
//! File ops cannot be cancelled: if the caller is cancelled, the op runs to completion on its thread
//! anyway; until then it counts as pending, so tenma does not exit in the middle of a write.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use serde_json::json;

use crate::ops;
use crate::php::{OpResult, bytes};

fn path(raw: &[u8]) -> PathBuf {
    OsStr::from_bytes(raw).into()
}

fn json(value: serde_json::Value) -> OpResult {
    OpResult::Text(value.to_string().into_bytes())
}

/// Like PHP copy(): copying a file onto itself would otherwise empty it.
fn same_file(from: &PathBuf, to: &PathBuf) -> bool {
    match (std::fs::metadata(from), std::fs::metadata(to)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

fn run(
    op: &str,
    target: PathBuf,
    second: PathBuf,
    flag: bool,
    data: Vec<u8>,
    room: usize,
) -> io::Result<OpResult> {
    match op {
        // Refused before Rust holds a file that PHP could not take under memory_limit
        "read" => {
            if std::fs::metadata(&target)?.len() > room as u64 {
                return Err(io::Error::other("the file does not fit under memory_limit"));
            }
            std::fs::read(&target).map(OpResult::Text)
        }
        "write" => std::fs::write(&target, data).map(|()| OpResult::Null),
        // One write call with O_APPEND: not in pieces that could mix with others
        "append" => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&target)?;
            file.write_all(&data).map(|()| OpResult::Null)
        }
        // Like file_exists(): missing permissions mean false, not an exception
        "exists" => Ok(json(json!(std::fs::metadata(&target).is_ok()))),
        "stat" => {
            let meta = std::fs::metadata(&target)?;
            let modified = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0.0, |since| since.as_secs_f64());
            Ok(json(
                json!({ "size": meta.len(), "modified": modified, "isFile": meta.is_file(), "isDir": meta.is_dir() }),
            ))
        }
        // Kind (f, d, o) and name as bytes, separated by null bytes: file names need not be valid
        // UTF-8. A link counts as what it points to; a dangling one is "other"
        "readDir" => {
            let mut names: Vec<Vec<u8>> = std::fs::read_dir(&target)?
                .map(|entry| {
                    let entry = entry?;
                    let kind = match std::fs::metadata(entry.path()) {
                        Ok(meta) if meta.is_file() => b'f',
                        Ok(meta) if meta.is_dir() => b'd',
                        _ => b'o',
                    };
                    let mut name = entry.file_name().as_bytes().to_vec();
                    name.insert(0, kind);
                    Ok(name)
                })
                .collect::<io::Result<_>>()?;
            names.sort_by(|a, b| a[1..].cmp(&b[1..]));
            Ok(OpResult::Text(names.join(&0)))
        }
        "mkdir" => if flag {
            std::fs::create_dir_all(&target)
        } else {
            std::fs::create_dir(&target)
        }
        .map(|()| OpResult::Null),
        "rename" => match std::fs::rename(&target, &second) {
            // Like PHP rename(): across file systems, copy and then delete
            Err(error)
                if error.raw_os_error() == Some(libc::EXDEV)
                    && std::fs::metadata(&target)?.is_file() =>
            {
                std::fs::copy(&target, &second)?;
                std::fs::remove_file(&target).map(|()| OpResult::Null)
            }
            result => result.map(|()| OpResult::Null),
        },
        "copy" => {
            if same_file(&target, &second) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source and destination are the same file",
                ));
            }
            Ok(json(json!(std::fs::copy(&target, &second)?)))
        }
        "remove" => std::fs::remove_file(&target).map(|()| OpResult::Null),
        "removeDir" => if flag {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_dir(&target)
        }
        .map(|()| OpResult::Null),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown op {other}"),
        )),
    }
}

/// Starts a file system op. $second is the destination for rename and copy; $flag means recursive for
/// mkdir and removeDir. Errors go to PHP with errno as the code.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_fs(
    op: *const u8,
    op_len: usize,
    target: *const u8,
    target_len: usize,
    second: *const u8,
    second_len: usize,
    flag: bool,
    data: *const u8,
    data_len: usize,
) -> u64 {
    let op = String::from_utf8_lossy(bytes(op, op_len)).into_owned();
    let (target, second) = (
        path(bytes(target, target_len)),
        path(bytes(second, second_len)),
    );
    let data = bytes(data, data_len).to_vec();
    let detail = format!("{op} {}", target.display());
    let room = crate::php::memory_room();
    ops::spawn_uncancellable("fs", detail, async move {
        let label = if second.as_os_str().is_empty() {
            target.display().to_string()
        } else {
            format!("{} -> {}", target.display(), second.display())
        };
        let job = {
            let op = op.clone();
            tokio::task::spawn_blocking(move || run(&op, target, second, flag, data, room))
        };
        match job.await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                // tenma's own errors (e.g. copy onto itself) get EINVAL, as for invalid arguments
                let code = error.raw_os_error().unwrap_or(
                    if error.kind() == io::ErrorKind::InvalidInput {
                        libc::EINVAL
                    } else {
                        0
                    },
                );
                OpResult::Failure {
                    message: format!("{op}({label}): {error}"),
                    code,
                }
            }
            Err(error) => OpResult::Error(format!("{op}({label}): {error}")),
        }
    })
}
