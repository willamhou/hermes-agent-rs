use std::path::{Path, PathBuf};

use hermes_core::tool::ToolConfig;

/// Resolve path: handles ~/ expansion, relative to working_dir.
pub fn resolve_path(path: &str, working_dir: &Path) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        working_dir.join(p)
    }
}

/// Lexically normalize a path by resolving `.` and `..` components without
/// hitting the filesystem. This is used as a fallback when `canonicalize` fails
/// (e.g. the path does not exist yet) to ensure `..`-based traversal attacks are
/// caught even when the target path is absent.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut components: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last normal component if possible; otherwise keep the `..`
                // so the resulting path is still "more traversed" than any workspace root.
                match components.last() {
                    Some(Component::Normal(_)) => {
                        components.pop();
                    }
                    _ => components.push(component),
                }
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Check resolved path is under workspace_root. Returns Err(String) if escapes.
/// For files that don't exist yet: canonicalize parent + file_name, then fall back
/// to lexical normalization so that `..`-traversal attacks are caught even when
/// the target path is absent on disk.
///
/// Also detects dangling symlinks (symlinks whose target does not exist) and
/// resolves them so they cannot be used to escape the sandbox.
pub fn check_sandbox(resolved: &Path, workspace_root: &Path) -> Result<(), String> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());

    // Detect dangling symlinks: symlink_metadata succeeds but metadata (which
    // follows symlinks) fails.  Read the link target and check it directly.
    let lmeta = std::fs::symlink_metadata(resolved);
    if let Ok(meta) = &lmeta {
        if meta.file_type().is_symlink() {
            // Follow the symlink target (may be relative)
            let target = std::fs::read_link(resolved).unwrap_or_else(|_| resolved.to_path_buf());
            let target_abs = if target.is_absolute() {
                target
            } else {
                resolved.parent().unwrap_or(Path::new(".")).join(&target)
            };
            // Recurse: check the resolved symlink target
            return check_sandbox(&target_abs, workspace_root);
        }
    }

    let canonical_path = if resolved.exists() {
        resolved
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(resolved))
    } else {
        // For files that don't exist yet: canonicalize parent + file_name,
        // then fall back to lexical normalization to catch `..` traversal.
        let parent = resolved.parent().unwrap_or(Path::new("."));
        let file_name = resolved.file_name().unwrap_or_default();
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| normalize_path(parent));
        canonical_parent.join(file_name)
    };

    if canonical_path.starts_with(&canonical_root) {
        Ok(())
    } else {
        Err(format!(
            "path '{}' escapes workspace root '{}'",
            resolved.display(),
            canonical_root.display()
        ))
    }
}

/// Check if path is a blocked device (/dev/zero, /dev/stdin, /proc/*/fd/*, etc.)
pub fn is_blocked_device(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("/dev/") {
        return true;
    }

    // Block /proc/*/fd/* (file descriptors)
    let components: Vec<_> = path.components().collect();
    if components.len() >= 2 {
        let first = components[0].as_os_str().to_string_lossy();
        let second = components
            .get(1)
            .map(|c| c.as_os_str().to_string_lossy())
            .unwrap_or_default();
        if first == "/" && second == "proc" {
            // /proc/*/fd/* pattern
            if components.len() >= 4 {
                let fourth = components
                    .get(3)
                    .map(|c| c.as_os_str().to_string_lossy())
                    .unwrap_or_default();
                if fourth == "fd" {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if file has binary extension (.png, .exe, .db, etc.)
pub fn has_binary_extension(path: &Path) -> bool {
    let binary_extensions = [
        "png", "jpg", "jpeg", "gif", "bmp", "tiff", "tif", "webp", "ico", "svg", "heic", "heif",
        "raw", "cr2", "nef", "orf", "sr2", "exe", "dll", "so", "dylib", "lib", "a", "o", "obj",
        "bin", "elf", "com", "msi", "deb", "rpm", "dmg", "pkg", "apk", "ipa", "appimage", "db",
        "sqlite", "sqlite3", "mdb", "accdb", "ldb", "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
        "zst", "lz4", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp",
        "mp3", "mp4", "avi", "mkv", "mov", "flac", "ogg", "wav", "aac", "m4a", "m4v", "webm",
        "ttf", "otf", "woff", "woff2", "eot", "class", "pyc", "pyo", "wasm", "iso", "img",
    ];

    if let Some(ext) = path.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        return binary_extensions.contains(&ext_lower.as_str());
    }
    false
}

/// Check if path is in blocked write prefixes (/etc/, /boot/, docker.sock, etc.)
pub fn is_blocked_write_path(path: &Path, config: &ToolConfig) -> bool {
    let path_str = path.to_string_lossy();

    // Hard-coded blocked paths
    let hard_blocked = [
        "/etc/",
        "/boot/",
        "/usr/lib/systemd/",
        "/sys/",
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/proc/",
        "/dev/",
    ];

    for blocked in &hard_blocked {
        if path_str.starts_with(blocked) || path_str == blocked.trim_end_matches('/') {
            return true;
        }
    }

    // Check config-provided blocked prefixes
    for prefix in &config.file.blocked_prefixes {
        if path.starts_with(prefix) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::tool::ToolConfig;
    use std::path::{Path, PathBuf};

    // ── resolve_path ──────────────────────────────────────────────────────────

    #[test]
    fn test_resolve_path_relative() {
        let working_dir = Path::new("/workspace/project");
        let result = resolve_path("src/main.rs", working_dir);
        assert_eq!(result, PathBuf::from("/workspace/project/src/main.rs"));
    }

    #[test]
    fn test_resolve_path_absolute() {
        let working_dir = Path::new("/workspace/project");
        let result = resolve_path("/usr/local/bin/rust", working_dir);
        assert_eq!(result, PathBuf::from("/usr/local/bin/rust"));
    }

    #[test]
    fn test_resolve_path_tilde() {
        let working_dir = Path::new("/workspace/project");
        let result = resolve_path("~/Documents/file.txt", working_dir);
        // Just verify it doesn't start with ~ and is absolute
        assert!(!result.to_string_lossy().starts_with('~'));
        assert!(result.is_absolute());
        assert!(result.to_string_lossy().ends_with("Documents/file.txt"));
    }

    #[test]
    fn test_resolve_path_already_absolute() {
        let working_dir = Path::new("/some/other/dir");
        let result = resolve_path("/absolute/path/file.rs", working_dir);
        assert_eq!(result, PathBuf::from("/absolute/path/file.rs"));
    }

    #[test]
    fn test_resolve_path_dot_relative() {
        let working_dir = Path::new("/workspace");
        let result = resolve_path("./subdir/file.txt", working_dir);
        assert_eq!(result, PathBuf::from("/workspace/./subdir/file.txt"));
    }

    // ── check_sandbox ─────────────────────────────────────────────────────────

    #[test]
    fn test_check_sandbox_path_inside_workspace() {
        // /tmp is a real directory that can be canonicalized
        let workspace_root = Path::new("/tmp");
        let resolved = Path::new("/tmp/subdir/file.rs");
        // This may not be canonical, but the function handles non-existent paths
        let result = check_sandbox(resolved, workspace_root);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_sandbox_path_outside_workspace() {
        let workspace_root = Path::new("/tmp");
        let resolved = Path::new("/etc/passwd");
        let result = check_sandbox(resolved, workspace_root);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("escapes workspace root"));
    }

    #[test]
    fn test_check_sandbox_parent_dir() {
        // Path trying to traverse up out of workspace
        let workspace_root = Path::new("/tmp");
        let resolved = Path::new("/home/user/secret.txt");
        let result = check_sandbox(resolved, workspace_root);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_sandbox_workspace_root_itself() {
        let workspace_root = Path::new("/tmp");
        let result = check_sandbox(workspace_root, workspace_root);
        assert!(result.is_ok());
    }

    // ── is_blocked_device ────────────────────────────────────────────────────

    #[test]
    fn test_is_blocked_device_dev_zero() {
        assert!(is_blocked_device(Path::new("/dev/zero")));
    }

    #[test]
    fn test_is_blocked_device_dev_sda() {
        assert!(is_blocked_device(Path::new("/dev/sda")));
    }

    #[test]
    fn test_is_blocked_device_dev_stdin() {
        assert!(is_blocked_device(Path::new("/dev/stdin")));
    }

    #[test]
    fn test_is_blocked_device_tmp_file() {
        assert!(!is_blocked_device(Path::new("/tmp/myfile.txt")));
    }

    #[test]
    fn test_is_blocked_device_normal_path() {
        assert!(!is_blocked_device(Path::new("/home/user/project/main.rs")));
    }

    #[test]
    fn test_is_blocked_device_proc_fd() {
        assert!(is_blocked_device(Path::new("/proc/1234/fd/3")));
    }

    // ── has_binary_extension ─────────────────────────────────────────────────

    #[test]
    fn test_has_binary_extension_png() {
        assert!(has_binary_extension(Path::new("image.png")));
    }

    #[test]
    fn test_has_binary_extension_exe() {
        assert!(has_binary_extension(Path::new("program.exe")));
    }

    #[test]
    fn test_has_binary_extension_db() {
        assert!(has_binary_extension(Path::new("data.db")));
    }

    #[test]
    fn test_has_binary_extension_rs_false() {
        assert!(!has_binary_extension(Path::new("main.rs")));
    }

    #[test]
    fn test_has_binary_extension_no_extension() {
        assert!(!has_binary_extension(Path::new("Makefile")));
    }

    #[test]
    fn test_has_binary_extension_txt_false() {
        assert!(!has_binary_extension(Path::new("readme.txt")));
    }

    // ── is_blocked_write_path ────────────────────────────────────────────────

    #[test]
    fn test_is_blocked_write_path_etc_passwd() {
        let config = ToolConfig::default();
        assert!(is_blocked_write_path(Path::new("/etc/passwd"), &config));
    }

    #[test]
    fn test_is_blocked_write_path_etc_dir() {
        let config = ToolConfig::default();
        assert!(is_blocked_write_path(
            Path::new("/etc/ssh/sshd_config"),
            &config
        ));
    }

    #[test]
    fn test_is_blocked_write_path_boot() {
        let config = ToolConfig::default();
        assert!(is_blocked_write_path(
            Path::new("/boot/grub/grub.cfg"),
            &config
        ));
    }

    #[test]
    fn test_is_blocked_write_path_tmp_file_ok() {
        let config = ToolConfig::default();
        assert!(!is_blocked_write_path(
            Path::new("/tmp/myfile.txt"),
            &config
        ));
    }

    #[test]
    fn test_is_blocked_write_path_home_ok() {
        let config = ToolConfig::default();
        assert!(!is_blocked_write_path(
            Path::new("/home/user/project/main.rs"),
            &config
        ));
    }

    #[test]
    fn test_is_blocked_write_path_docker_sock() {
        let config = ToolConfig::default();
        assert!(is_blocked_write_path(
            Path::new("/var/run/docker.sock"),
            &config
        ));
    }
}
