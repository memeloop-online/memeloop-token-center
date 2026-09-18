use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
#[cfg(feature = "experimental-plugin-revisions")]
use std::{process::Stdio, time::Duration};

#[cfg(feature = "plugin-distribution")]
pub const COSIGN_VERIFIER_FILENAME: &str = "cosign";
#[cfg(feature = "plugin-distribution")]
pub const COSIGN_VERIFIER_VERSION: &str = "v3.1.3-mtc.3";
#[cfg(feature = "experimental-plugin-revisions")]
const PLUGIN_INSTALLER_FILENAME: &str = "install-plugin-oci";

#[cfg(feature = "experimental-plugin-revisions")]
struct PluginRuntimeLayout {
    binary_directory: PathBuf,
    library_directory: PathBuf,
    installer: PathBuf,
}

fn executable_regular_sibling(
    directory: &Path,
    filename: &str,
    description: &str,
) -> io::Result<PathBuf> {
    let candidate = directory.join(filename);
    let metadata = std::fs::symlink_metadata(&candidate)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(io::Error::other(format!(
            "{description} must be an executable regular file"
        )));
    }
    let canonical = candidate.canonicalize()?;
    if canonical.parent() != Some(directory) {
        return Err(io::Error::other(format!(
            "{description} escaped the executable directory"
        )));
    }
    Ok(canonical)
}

#[cfg(feature = "experimental-plugin-revisions")]
fn plugin_runtime_layout_from(executable: &Path) -> io::Result<PluginRuntimeLayout> {
    let binary_directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("runtime executable directory unavailable"))?
        .canonicalize()?;
    let root = binary_directory
        .parent()
        .ok_or_else(|| io::Error::other("runtime root unavailable"))?;
    let installer = executable_regular_sibling(
        &binary_directory,
        PLUGIN_INSTALLER_FILENAME,
        "co-located plugin installer",
    )?;
    let library_candidate = root.join("lib");
    let library_metadata = std::fs::symlink_metadata(&library_candidate)?;
    if !library_metadata.is_dir() || library_metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "co-located runtime library path must be a regular directory",
        ));
    }
    let library_directory = library_candidate.canonicalize()?;
    if library_directory.parent() != Some(root) {
        return Err(io::Error::other(
            "co-located runtime library path escaped the package root",
        ));
    }
    Ok(PluginRuntimeLayout {
        installer,
        library_directory,
        binary_directory,
    })
}

#[cfg(feature = "experimental-plugin-revisions")]
pub(crate) fn plugin_installer_command() -> io::Result<tokio::process::Command> {
    let runtime = plugin_runtime_layout_from(&std::env::current_exe()?)?;
    let search_path = std::env::join_paths([
        runtime.binary_directory.as_path(),
        Path::new("/usr/bin"),
        Path::new("/bin"),
    ])
    .map_err(|_| io::Error::other("plugin runtime search path is invalid"))?;
    let mut command = tokio::process::Command::new(runtime.installer);
    command
        .env_clear()
        .env("LD_LIBRARY_PATH", runtime.library_directory)
        .env("PATH", search_path);
    Ok(command)
}

#[cfg(feature = "experimental-plugin-revisions")]
pub(crate) async fn verify_plugin_runtime() -> io::Result<()> {
    let mut command = plugin_installer_command()?;
    command
        .arg("--mtc-cosign-runtime-check")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(15), command.status())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "plugin runtime check timed out"))??;
    if !status.success() {
        return Err(io::Error::other("plugin runtime check failed"));
    }
    Ok(())
}

#[cfg(feature = "plugin-distribution")]
pub(crate) fn cosign_verifier_path() -> io::Result<PathBuf> {
    cosign_verifier_path_from(&std::env::current_exe()?)
}

#[cfg(feature = "plugin-distribution")]
fn cosign_verifier_path_from(executable: &Path) -> io::Result<PathBuf> {
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("installer executable directory unavailable"))?
        .canonicalize()?;
    executable_regular_sibling(&directory, COSIGN_VERIFIER_FILENAME, "co-located cosign")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    #[test]
    #[cfg(feature = "plugin-distribution")]
    fn cosign_binary_is_resolved_only_as_an_executable_regular_sibling() {
        let temporary = TempDir::new().expect("temporary directory");
        let executable = temporary.path().join("install-plugin-oci");
        std::fs::write(&executable, b"installer").expect("installer fixture");
        let cosign = temporary.path().join(COSIGN_VERIFIER_FILENAME);
        std::fs::write(&cosign, b"cosign").expect("cosign fixture");
        std::fs::set_permissions(&cosign, std::fs::Permissions::from_mode(0o500))
            .expect("executable cosign fixture");
        assert_eq!(
            cosign_verifier_path_from(&executable).expect("co-located cosign"),
            cosign.canonicalize().expect("canonical cosign")
        );

        std::fs::remove_file(&cosign).expect("remove regular cosign");
        let external = temporary.path().join("external-cosign");
        std::fs::write(&external, b"cosign").expect("external fixture");
        std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o500))
            .expect("external executable fixture");
        symlink(&external, &cosign).expect("symlink fixture");
        assert!(cosign_verifier_path_from(&executable).is_err());
    }

    #[test]
    #[cfg(feature = "experimental-plugin-revisions")]
    fn service_runtime_resolves_installer_and_libraries_relative_to_its_binary() {
        let temporary = TempDir::new().expect("temporary directory");
        let bin = temporary.path().join("bin");
        std::fs::create_dir(&bin).expect("bin directory");
        let lib = temporary.path().join("lib");
        std::fs::create_dir(&lib).expect("lib directory");
        let executable = bin.join("memeloop-token-center.bin");
        std::fs::write(&executable, b"service").expect("service fixture");
        let installer = bin.join(PLUGIN_INSTALLER_FILENAME);
        std::fs::write(&installer, b"installer").expect("installer fixture");
        std::fs::set_permissions(&installer, std::fs::Permissions::from_mode(0o500))
            .expect("executable installer fixture");
        let runtime = plugin_runtime_layout_from(&executable).expect("runtime layout");
        assert_eq!(runtime.binary_directory, bin.canonicalize().unwrap());
        assert_eq!(runtime.installer, installer.canonicalize().unwrap());
        assert_eq!(runtime.library_directory, lib.canonicalize().unwrap());
    }
}
