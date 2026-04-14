use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

use hermes_core::{
    stream::StreamDelta,
    tool::{
        ApprovalRequest, BrowserToolConfig, FileToolConfig, TerminalToolConfig, Tool, ToolConfig,
        ToolContext,
    },
};

fn make_test_ctx(workspace: &Path) -> ToolContext {
    let (approval_tx, _) = tokio::sync::mpsc::channel::<ApprovalRequest>(1);
    let (delta_tx, _) = tokio::sync::mpsc::channel::<StreamDelta>(1);
    ToolContext {
        session_id: "security-test".into(),
        working_dir: workspace.to_path_buf(),
        approval_tx,
        delta_tx,
        tool_config: Arc::new(ToolConfig {
            terminal: TerminalToolConfig::default(),
            file: FileToolConfig::default(),
            browser: BrowserToolConfig::default(),
            workspace_root: workspace.to_path_buf(),
        }),
        memory: None,
        aux_provider: None,
        skills: None,
    }
}

/// Attack paths that should ALL be blocked (return is_error == true).
const TRAVERSAL_PATHS: &[&str] = &[
    // Basic relative traversal
    "../../../etc/passwd",
    "../../.ssh/id_rsa",
    "../../../etc/shadow",
    // Multi-hop traversal
    "foo/../../../../../../etc/passwd",
    "./foo/../../../etc/passwd",
    // Absolute paths outside workspace
    "/etc/passwd",
    "/etc/hostname",
    "/root/.bashrc",
    "/home/../etc/passwd",
    // Tilde expansion to outside workspace
    "~/.ssh/id_rsa",
    "~/../../etc/passwd",
];

// ── Read file: traversal attacks ─────────────────────────────────────────────

#[tokio::test]
async fn test_read_file_traversal_attacks() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());
    let tool = hermes_tools::file_read::ReadFileTool;

    for attack_path in TRAVERSAL_PATHS {
        let args = serde_json::json!({"path": attack_path});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(
            result.is_error,
            "read_file: path '{}' should be blocked but wasn't; got: {}",
            attack_path, result.content
        );
    }
}

// ── Write file: traversal attacks ────────────────────────────────────────────

#[tokio::test]
async fn test_write_file_traversal_attacks() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());
    let tool = hermes_tools::file_write::WriteFileTool;

    for attack_path in TRAVERSAL_PATHS {
        let args = serde_json::json!({"path": attack_path, "content": "pwned"});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(
            result.is_error,
            "write_file: path '{}' should be blocked but wasn't; got: {}",
            attack_path, result.content
        );
    }
}

// ── Search files: traversal attacks ─────────────────────────────────────────

#[tokio::test]
async fn test_search_files_traversal_attacks() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());
    let tool = hermes_tools::file_search::SearchFilesTool;

    for attack_path in TRAVERSAL_PATHS {
        let args = serde_json::json!({"pattern": "root", "path": attack_path});
        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(
            result.is_error,
            "search_files: path '{}' should be blocked but wasn't; got: {}",
            attack_path, result.content
        );
    }
}

// ── Symlink attack: read file escaping workspace ──────────────────────────────

#[tokio::test]
async fn test_read_file_symlink_escape() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    // Symlink inside workspace → /etc/passwd (outside workspace)
    let evil_link = workspace.path().join("evil_link");
    std::os::unix::fs::symlink("/etc/passwd", &evil_link).unwrap();

    let tool = hermes_tools::file_read::ReadFileTool;
    let args = serde_json::json!({"path": "evil_link"});
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "symlink to /etc/passwd should be blocked; got: {}",
        result.content
    );
}

// ── Symlink attack: write file escaping workspace ────────────────────────────

#[tokio::test]
async fn test_write_file_symlink_escape() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    let target = "/tmp/hermes_symlink_attack_test_write";
    // Clean up from any prior run
    let _ = std::fs::remove_file(target);

    // Symlink inside workspace → /tmp/hermes_symlink_attack_test_write (outside workspace)
    let evil_link = workspace.path().join("evil_write");
    std::os::unix::fs::symlink(target, &evil_link).unwrap();

    let tool = hermes_tools::file_write::WriteFileTool;
    let args = serde_json::json!({"path": "evil_write", "content": "pwned"});
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "symlink write escape should be blocked; got: {}",
        result.content
    );
    // Verify nothing was written to the target outside workspace
    assert!(
        !Path::new(target).exists(),
        "target file should not have been created at {}",
        target
    );
}

// ── Symlink within workspace: should be allowed ──────────────────────────────

#[tokio::test]
async fn test_read_file_symlink_within_workspace_ok() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    // Real file inside workspace
    let real_file = workspace.path().join("real.txt");
    std::fs::write(&real_file, "hello").unwrap();

    // Symlink inside workspace → real file inside workspace
    let link = workspace.path().join("link.txt");
    std::os::unix::fs::symlink(&real_file, &link).unwrap();

    let tool = hermes_tools::file_read::ReadFileTool;
    let args = serde_json::json!({"path": "link.txt"});
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        !result.is_error,
        "symlink within workspace should be allowed; got: {}",
        result.content
    );
    assert!(
        result.content.contains("hello"),
        "expected file content 'hello'; got: {}",
        result.content
    );
}

// ── Symlink directory attack: search escaping workspace ──────────────────────

#[tokio::test]
async fn test_search_symlink_directory_escape() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    // Symlink directory inside workspace → /etc
    let evil_dir = workspace.path().join("evil_dir");
    std::os::unix::fs::symlink("/etc", &evil_dir).unwrap();

    let tool = hermes_tools::file_search::SearchFilesTool;
    let args = serde_json::json!({"pattern": "root", "path": "evil_dir"});
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "search through symlinked directory to /etc should be blocked; got: {}",
        result.content
    );
}

// ── Terminal workdir symlink attack ──────────────────────────────────────────

#[tokio::test]
async fn test_terminal_workdir_symlink_escape() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    // Symlink directory inside workspace → /etc
    let evil_workdir = workspace.path().join("evil_workdir");
    std::os::unix::fs::symlink("/etc", &evil_workdir).unwrap();

    let tool = hermes_tools::terminal::TerminalTool;
    let args = serde_json::json!({
        "command": "cat passwd",
        "workdir": evil_workdir.to_str().unwrap()
    });
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "terminal with symlinked workdir pointing to /etc should be blocked; got: {}",
        result.content
    );
}

// ── Terminal workdir absolute-path traversal ─────────────────────────────────

#[tokio::test]
async fn test_terminal_workdir_traversal() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());

    let tool = hermes_tools::terminal::TerminalTool;
    let args = serde_json::json!({
        "command": "pwd",
        "workdir": "/etc"
    });
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "terminal workdir /etc should be blocked; got: {}",
        result.content
    );
}

// ── Null-byte path (should be caught by sandbox or OS rejection) ─────────────

#[tokio::test]
async fn test_read_file_null_byte_path() {
    let workspace = TempDir::new().unwrap();
    let ctx = make_test_ctx(workspace.path());
    let tool = hermes_tools::file_read::ReadFileTool;

    // Null byte in path — OS will reject, or sandbox check will catch /etc/passwd prefix
    let args = serde_json::json!({"path": "/etc/passwd\x00.txt"});
    let result = tool.execute(args, &ctx).await.unwrap();

    // Either blocked by sandbox (path starts with /etc) or fails due to null byte
    assert!(
        result.is_error,
        "null-byte path should be blocked; got: {}",
        result.content
    );
}

// ── Write to /etc via absolute path (double-check blocked write paths) ────────

#[tokio::test]
async fn test_write_file_etc_blocked() {
    // Use /tmp as workspace so sandbox check doesn't fire first
    let (approval_tx, _) = tokio::sync::mpsc::channel::<ApprovalRequest>(1);
    let (delta_tx, _) = tokio::sync::mpsc::channel::<StreamDelta>(1);
    let ctx = ToolContext {
        session_id: "security-test".into(),
        working_dir: std::path::PathBuf::from("/tmp"),
        approval_tx,
        delta_tx,
        tool_config: Arc::new(ToolConfig {
            terminal: TerminalToolConfig::default(),
            file: FileToolConfig::default(),
            browser: BrowserToolConfig::default(),
            workspace_root: std::path::PathBuf::from("/tmp"),
        }),
        memory: None,
        aux_provider: None,
        skills: None,
    };

    let tool = hermes_tools::file_write::WriteFileTool;
    let args = serde_json::json!({"path": "/etc/hermes_attack_test", "content": "pwned"});
    let result = tool.execute(args, &ctx).await.unwrap();

    assert!(
        result.is_error,
        "write to /etc/ should be blocked; got: {}",
        result.content
    );
    assert!(
        !Path::new("/etc/hermes_attack_test").exists(),
        "file should not have been created in /etc"
    );
}
