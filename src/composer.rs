//! Composer for `tenma composer …`: `tenma install composer` fetches the latest stable phar from
//! getcomposer.org into the user's data directory, `update` fetches it again, `remove` deletes it.

use std::path::PathBuf;

const URL: &str = "https://getcomposer.org/download/latest-stable/composer.phar";

/// $XDG_DATA_HOME/tenma/composer.phar, or ~/.local/share/tenma/composer.phar.
fn path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("tenma/composer.phar")
}

/// The phar `tenma composer` runs as its script.
pub fn script() -> Result<String, String> {
    let path = path();
    if !path.is_file() {
        return Err("Composer is not installed: tenma install composer".into());
    }
    Ok(path.display().to_string())
}

/// `tenma install|update|remove composer`: what happened, for the terminal.
pub fn manage(action: &str) -> Result<String, String> {
    let path = path();
    match action {
        "install" if path.is_file() => Err(format!(
            "Composer is already installed in {}; tenma update composer fetches the latest",
            path.display()
        )),
        "update" | "remove" if !path.is_file() => {
            Err("Composer is not installed: tenma install composer".into())
        }
        "install" => {
            fetch(&path)?;
            Ok(format!("Composer installed in {}", path.display()))
        }
        "update" => {
            fetch(&path)?;
            Ok(format!("Composer updated in {}", path.display()))
        }
        _ => {
            std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            Ok(format!("Composer removed from {}", path.display()))
        }
    }
}

/// Downloads the phar, checks it against the checksum getcomposer.org publishes and puts it in
/// place.
fn fetch(path: &std::path::Path) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let (phar, checksum) = runtime
        .block_on(async {
            let phar = reqwest::get(URL).await?.error_for_status()?.bytes().await?;
            let checksum = reqwest::get(format!("{URL}.sha256"))
                .await?
                .error_for_status()?
                .text()
                .await?;
            Ok::<_, reqwest::Error>((phar, checksum))
        })
        .map_err(|error| format!("{URL}: {error}"))?;

    let digest = ring::digest::digest(&ring::digest::SHA256, &phar);
    let actual: String = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let expected = checksum.split_whitespace().next().unwrap_or_default();
    if actual != expected {
        return Err(format!("{URL}: checksum {actual} is not {expected}"));
    }

    // Written beside it and renamed, so a running tenma composer never sees half a file
    let partial = path.with_extension(format!("phar.{}", std::process::id()));
    let directory = path.parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(directory)
        .and_then(|()| std::fs::write(&partial, &phar))
        .and_then(|()| std::fs::rename(&partial, path))
        .map_err(|error| format!("{}: {error}", path.display()))
}
