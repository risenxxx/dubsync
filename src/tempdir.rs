use crate::error::Result;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Workspace for intermediate WAV files. When `keep_temp` is true, the dir is
/// leaked into the filesystem on drop so the user can inspect it.
pub struct Workspace {
    inner: WorkspaceInner,
    keep: bool,
}

enum WorkspaceInner {
    Owned(TempDir),
    Persistent(PathBuf),
}

impl Workspace {
    pub fn new(custom_root: Option<&Path>, keep_temp: bool) -> Result<Self> {
        let inner = match custom_root {
            Some(root) => {
                std::fs::create_dir_all(root)?;
                let td = tempfile::Builder::new()
                    .prefix("dubsync-")
                    .tempdir_in(root)?;
                WorkspaceInner::Owned(td)
            }
            None => {
                let td = tempfile::Builder::new().prefix("dubsync-").tempdir()?;
                WorkspaceInner::Owned(td)
            }
        };
        Ok(Self {
            inner,
            keep: keep_temp,
        })
    }

    pub fn path(&self) -> &Path {
        match &self.inner {
            WorkspaceInner::Owned(td) => td.path(),
            WorkspaceInner::Persistent(p) => p,
        }
    }

    pub fn child(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if !self.keep {
            return;
        }
        // Replace the Owned variant so its destructor doesn't delete the dir.
        let placeholder = WorkspaceInner::Persistent(PathBuf::new());
        let owned = std::mem::replace(&mut self.inner, placeholder);
        if let WorkspaceInner::Owned(td) = owned {
            let path = td.keep();
            tracing::info!(workspace = %path.display(), "kept temp workspace (--keep-temp)");
            self.inner = WorkspaceInner::Persistent(path);
        }
    }
}
