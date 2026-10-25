// Builds the C layer against the self-built PHP (php-build/build.sh) and links libphp.a statically.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let php = root.join("php-build/out");
    let config = php.join("bin/php-config");
    assert!(config.exists(), "PHP missing: run php-build/build.sh first");

    let run = |option: &str| -> String {
        let output = Command::new(&config)
            .arg(option)
            .output()
            .expect("php-config does not run");
        String::from_utf8(output.stdout).unwrap()
    };
    // php-config reports the absolute path from when PHP was built: after the repo moves it points
    // nowhere, so redirect it to the current php-build/out
    let built_at = run("--prefix").trim().to_owned();
    let flags = |option: &str| -> Vec<String> {
        run(option)
            .split_whitespace()
            .map(|flag| flag.replace(&built_at, &php.display().to_string()))
            .collect()
    };

    // For tenma --version
    println!(
        "cargo:rustc-env=TENMA_PHP_VERSION={}",
        run("--version").trim()
    );

    let mut build = cc::Build::new();
    build.file("shim/tenma.c").warnings(false).define(
        "TENMA_VERSION",
        format!("\"{}\"", std::env::var("CARGO_PKG_VERSION").unwrap()).as_str(),
    );
    for include in flags("--includes") {
        build.include(include.trim_start_matches("-I"));
    }
    build.compile("tenma_shim");

    println!(
        "cargo:rustc-link-search=native={}",
        php.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=php");
    // What libphp.a itself needs: search paths, libraries and macOS frameworks of the extensions
    let mut linker = [flags("--ldflags"), flags("--libs")].concat().into_iter();
    while let Some(flag) = linker.next() {
        if let Some(path) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={}", unversioned(path));
        } else if let Some(name) = flag.strip_prefix("-l") {
            // intl is C++; PHP calls the runtime stdc++, on macOS it is called c++
            let name = if name == "stdc++" && cfg!(target_os = "macos") {
                "c++"
            } else {
                name
            };
            println!("cargo:rustc-link-lib={name}");
        } else if flag == "-framework" {
            if let Some(name) = linker.next() {
                println!("cargo:rustc-link-lib=framework={name}");
            }
        } else if flag.starts_with("-Wl,") {
            println!("cargo:rustc-link-arg={flag}");
        }
    }
    // The version in Cargo.toml goes into the binary
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=shim/tenma.c");
    println!("cargo:rerun-if-changed=php-build/out/lib/libphp.a");

    embed_stdlib(&root);
}

/// Lists every PHP file of stdlib/ for src/stdlib.rs, so the binary embeds them all.
fn embed_stdlib(root: &std::path::Path) {
    let stdlib = root.join("stdlib");
    let mut files = Vec::new();
    collect_php(&stdlib, &mut files);
    files.sort();

    let mut list = String::from("&[\n");
    for file in &files {
        let relative = file
            .strip_prefix(&stdlib)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        list.push_str(&format!(
            "    ({relative:?}, include_str!({:?})),\n",
            file.display().to_string()
        ));
    }
    list.push(']');

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("stdlib.rs");
    std::fs::write(out, list).expect("cannot write stdlib.rs");
    // A directory here is scanned as a whole: a new or removed file runs this again
    println!("cargo:rerun-if-changed=stdlib");
}

fn collect_php(directory: &std::path::Path, files: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory).expect("cannot read stdlib/");
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_php(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "php") {
            files.push(path);
        }
    }
}

/// pkg-config hands PHP Homebrew's versioned paths (/opt/homebrew/Cellar/libzip/1.11.4_1/lib), which are gone
/// after a `brew upgrade`; /opt/homebrew/opt/libzip/lib always points to the installed version.
fn unversioned(path: &str) -> String {
    let Some((prefix, rest)) = path.split_once("/Cellar/") else {
        return path.to_owned();
    };
    let mut parts = rest.splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(package), Some(_version), Some(tail)) => format!("{prefix}/opt/{package}/{tail}"),
        (Some(package), Some(_version), None) => format!("{prefix}/opt/{package}"),
        _ => path.to_owned(),
    }
}
