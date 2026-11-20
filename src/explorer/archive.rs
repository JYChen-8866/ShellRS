//! The system archivers the right sidebar's 文件 runs over the terminal's
//! SSH connection. Commands always start in the pane's directory, quote every
//! name as one shell word, and print a marker only after the archiver succeeds.

const DONE_MARKER: &str = "__SHELLRS_ARCHIVE_DONE__";
const TAR_SUFFIXES: &[&str] = &[
    ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz", ".tbz2", ".tar.xz", ".txz", ".tar.zst", ".tzst",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveFormat {
    Zip,
    TarGz,
}

/// A name typed in the compression dialog, with `.zip` added when it does
/// not already name one of the formats the system archivers create.
pub(crate) fn normalize_archive_name(name: &str) -> String {
    let name = name.trim();
    if name.is_empty() || matches!(name, "." | "..") || archive_format(name).is_some() {
        name.to_string()
    } else {
        format!("{name}.zip")
    }
}

pub(crate) fn default_archive_name(names: &[String]) -> String {
    match names {
        [name] => format!("{name}.zip"),
        _ => "archive.zip".to_string(),
    }
}

pub(crate) fn archive_format(name: &str) -> Option<ArchiveFormat> {
    let name = name.to_ascii_lowercase();
    if name.ends_with(".zip") {
        Some(ArchiveFormat::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Some(ArchiveFormat::TarGz)
    } else {
        None
    }
}

pub(crate) fn is_extractable(name: &str) -> bool {
    extract_program(name).is_some()
}

pub(crate) fn archive_command(directory: &str, archive: &str, names: &[String]) -> Option<String> {
    if names.is_empty()
        || !safe_entry_name(archive)
        || names.iter().any(|name| !safe_entry_name(name))
    {
        return None;
    }
    let format = archive_format(archive)?;
    let directory = quote(directory);
    let archive = quote(&format!("./{archive}"));
    let names = names
        .iter()
        .map(|name| quote(&format!("./{name}")))
        .collect::<Vec<_>>()
        .join(" ");
    let command = match format {
        ArchiveFormat::Zip => format!("zip -qr {archive} {names}"),
        ArchiveFormat::TarGz => format!("tar -czf {archive} {names}"),
    };
    let script = format!("cd {directory} 2>&1 && ({command}) 2>&1 && printf '{DONE_MARKER}'");
    Some(format!("sh -c {}", quote(&script)))
}

pub(crate) fn extract_command(directory: &str, archive: &str) -> Option<String> {
    if !safe_entry_name(archive) {
        return None;
    }
    let program = extract_program(archive)?;
    let script = format!(
        "cd {} 2>&1 && ({} {}) 2>&1 && printf '{}'",
        quote(directory),
        program,
        quote(&format!("./{archive}")),
        DONE_MARKER
    );
    Some(format!("sh -c {}", quote(&script)))
}

pub(crate) fn command_succeeded(output: &str) -> bool {
    output.contains(DONE_MARKER)
}

pub(crate) fn command_output(output: &str) -> Option<String> {
    let output = output.replace(DONE_MARKER, "");
    let output = output.trim();
    (!output.is_empty()).then(|| output.to_string())
}

fn extract_program(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name.ends_with(".zip") {
        Some("unzip -nq")
    } else if TAR_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        Some("tar -xkf")
    } else if name.ends_with(".7z") {
        Some("7z x -aos -y")
    } else if name.ends_with(".rar") {
        Some("unrar x -o- -y")
    } else if name.ends_with(".gz") {
        Some("gzip -dk")
    } else if name.ends_with(".bz2") {
        Some("bzip2 -dk")
    } else if name.ends_with(".xz") {
        Some("xz -dk")
    } else if name.ends_with(".zst") {
        Some("zstd -dk")
    } else {
        None
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn safe_entry_name(name: &str) -> bool {
    !name.is_empty() && !matches!(name, "." | "..") && !name.contains(['/', '\0'])
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveFormat, archive_command, archive_format, command_output, command_succeeded,
        default_archive_name, extract_command, is_extractable, normalize_archive_name, quote,
    };

    #[test]
    fn archive_names_choose_zip_or_tar_gz() {
        assert_eq!(normalize_archive_name(" photos "), "photos.zip");
        assert_eq!(normalize_archive_name("photos.TAR.GZ"), "photos.TAR.GZ");
        assert_eq!(normalize_archive_name(" "), "");
        assert_eq!(normalize_archive_name(".."), "..");
        assert_eq!(archive_format("photos.zip"), Some(ArchiveFormat::Zip));
        assert_eq!(archive_format("photos.tgz"), Some(ArchiveFormat::TarGz));
        assert_eq!(default_archive_name(&["one.txt".into()]), "one.txt.zip");
        assert_eq!(
            default_archive_name(&["one".into(), "two".into()]),
            "archive.zip"
        );
    }

    #[test]
    fn commands_quote_paths_and_only_succeed_with_the_marker() {
        let command = archive_command(
            "/home/me/a folder",
            "backup.zip",
            &["one's.txt".into(), "two".into()],
        )
        .unwrap();
        assert!(command.starts_with("sh -c "));
        assert!(command.contains("/home/me/a folder"));
        assert!(command.contains("zip -qr "));
        assert_eq!(quote("one's.txt"), "'one'\\''s.txt'");
        assert!(!command_succeeded("zip: command not found"));
        assert!(command_succeeded("__SHELLRS_ARCHIVE_DONE__"));
        assert_eq!(
            command_output("zip: command not found\n__SHELLRS_ARCHIVE_DONE__"),
            Some("zip: command not found".into())
        );
    }

    #[test]
    fn extraction_uses_the_system_program_for_the_suffix() {
        assert!(is_extractable("a.zip"));
        assert!(is_extractable("a.tar.gz"));
        assert!(is_extractable("a.tgz"));
        assert!(is_extractable("a.7z"));
        assert!(!is_extractable("a.txt"));
        let command = extract_command("/tmp", "a.tar.gz").unwrap();
        assert!(command.contains("tar -xkf "));
        assert!(command.contains("a.tar.gz"));
    }
}
