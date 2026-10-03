#[cfg(any(target_os = "windows", target_os = "linux"))]
mod app;
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod platform;

const VERSION: &str = match option_env!("STORAGE_ANALYTICS_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let initial_path = match args.next() {
        Some(arg) if arg == "--version" || arg == "-V" => {
            println!("storage_analytics {VERSION}");
            return Ok(());
        }
        Some(arg) if arg == "--help" || arg == "-h" => {
            println!(
                "Storage Analytics {VERSION}\nUsage: storage_analytics [DIRECTORY]\n\nBrowse mounted filesystems, or start in DIRECTORY.\n--version, -V  Show the version\n--help, -h     Show this help"
            );
            return Ok(());
        }
        Some(arg) => {
            let path = std::fs::canonicalize(arg)?;
            if !path.is_dir() {
                return Err("The starting path must be a directory.".into());
            }
            Some(path)
        }
        None => None,
    };
    if args.next().is_some() {
        return Err("Expected at most one starting directory.".into());
    }
    app::run(initial_path)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn main() {
    eprintln!("Storage Analytics {VERSION} supports Windows and Linux.");
    std::process::exit(1);
}
