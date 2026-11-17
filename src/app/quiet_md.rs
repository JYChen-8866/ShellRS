//! Quiet.md, the local notes app, from the tool switch.

use std::process::{Command, Stdio};

/// Open the installed Quiet.md app, reusing the running instance when it has one.
pub fn open_quiet_md() -> std::io::Result<()> {
    let mut command = quiet_md_command();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command.spawn().map(|_| ())
}

#[cfg(target_os = "macos")]
fn quiet_md_command() -> Command {
    let mut command = Command::new("open");
    command.args(["-b", "com.s1dashu.quietmd"]);
    command
}

#[cfg(windows)]
fn quiet_md_command() -> Command {
    match quiet_md_windows_path() {
        Some(path) => Command::new(path),
        // A portable copy or another install can still be on PATH.
        None => Command::new("quiet_md.exe"),
    }
}

/// Where the Windows installer puts Quiet.md, plus the common per-machine
/// locations. Windows paths compare case-insensitively.
#[cfg(windows)]
fn quiet_md_windows_path() -> Option<std::path::PathBuf> {
    ["LOCALAPPDATA", "PROGRAMFILES", "PROGRAMFILES(X86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(std::path::PathBuf::from)
        .flat_map(|base| {
            let nested = base.join("Programs").join("quiet.md").join("quiet_md.exe");
            let program_files = base.join("Quiet.md").join("quiet_md.exe");
            [nested, program_files]
        })
        .find(|path| path.is_file())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn quiet_md_command() -> Command {
    Command::new("quiet_md")
}

#[cfg(test)]
mod tests {
    use super::quiet_md_command;

    #[test]
    fn launches_the_installed_quiet_md_app() {
        let command = quiet_md_command();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(command.get_program(), "open");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["-b", "com.s1dashu.quietmd"]
            );
        }
        #[cfg(windows)]
        assert!(
            command
                .get_program()
                .to_string_lossy()
                .ends_with("quiet_md.exe"),
            "unexpected program: {:?}",
            command.get_program()
        );
        #[cfg(not(any(target_os = "macos", windows)))]
        assert_eq!(command.get_program(), "quiet_md");
    }
}
