//! Which files an agent may hand a page (`upload`): regular files inside
//! the thread's workspace or the temporary folder, judged by where they
//! really are (links followed), and not hard-linked from elsewhere.

use std::path::{Path, PathBuf};

/// The folders uploads may come from: `roots` (the thread's workspace),
/// the system's temporary folder and, on Unix, `/tmp`; each as it really
/// is on disk (missing ones dropped), sorted, once each.
pub fn upload_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut all: Vec<PathBuf> = roots.to_vec();
    all.push(std::env::temp_dir());
    if cfg!(unix) {
        all.push(PathBuf::from("/tmp"));
    }
    let mut real: Vec<PathBuf> = all
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .map(plain)
        .collect();
    real.sort();
    real.dedup();
    real
}

/// Where `path` really is, if a page may be given it.
pub fn check_upload(path: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let listed = || {
        roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let given = path.trim();
    if given.is_empty() || !Path::new(given).is_absolute() {
        return Err(format!(
            "{given:?} is not an absolute path: give the full path of a file in {}",
            listed()
        ));
    }
    let real = std::fs::canonicalize(given)
        .map(plain)
        .map_err(|e| format!("{given} can't be uploaded: {e}"))?;
    if !roots
        .iter()
        .any(|root| real != *root && real.starts_with(root))
    {
        return Err(format!(
            "{given} is outside the folders files may be uploaded from ({}): copy it into the \
             workspace or the temporary folder first",
            listed()
        ));
    }
    let meta = std::fs::metadata(&real).map_err(|e| format!("{given} can't be uploaded: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{given} is not a file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.nlink() > 1 {
            return Err(format!(
                "{given} has other names elsewhere (hard links): copy it to a file of its own \
                 first"
            ));
        }
    }
    Ok(real)
}

/// A canonical path as the rest of the system writes it: on Windows,
/// without the `\\?\` that `canonicalize` puts before a drive letter.
fn plain(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && rest.as_bytes().get(1) == Some(&b':')
        {
            return PathBuf::from(rest);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_in_the_roots_only() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // Only the workspace (the temporary folder would hold `outside`).
        let roots = vec![std::fs::canonicalize(workspace.path()).unwrap()];
        let file = workspace.path().join("report.txt");
        std::fs::write(&file, "hello").unwrap();
        let real = check_upload(&format!(" {} ", file.display()), &roots).unwrap();
        assert_eq!(real, std::fs::canonicalize(&file).unwrap());

        assert!(check_upload("report.txt", &roots).is_err());
        assert!(check_upload("", &roots).is_err());
        let workspace_itself = check_upload(&workspace.path().to_string_lossy(), &roots);
        assert!(workspace_itself.is_err());
        let missing =
            check_upload(&workspace.path().join("nope.txt").to_string_lossy(), &roots).unwrap_err();
        assert!(missing.contains("nope.txt"), "{missing}");
        let elsewhere = outside.path().join("secret.txt");
        std::fs::write(&elsewhere, "secret").unwrap();
        let refused = check_upload(&elsewhere.to_string_lossy(), &roots).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
    }

    #[cfg(unix)]
    #[test]
    fn links_out_are_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = vec![std::fs::canonicalize(workspace.path()).unwrap()];
        let elsewhere = outside.path().join("secret.txt");
        std::fs::write(&elsewhere, "secret").unwrap();
        let link = workspace.path().join("link.txt");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        let refused = check_upload(&link.to_string_lossy(), &roots).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
        let hard = workspace.path().join("hard.txt");
        std::fs::hard_link(&elsewhere, &hard).unwrap();
        let refused = check_upload(&hard.to_string_lossy(), &roots).unwrap_err();
        assert!(refused.contains("hard links"), "{refused}");
        let refused = check_upload("/etc/hosts", &roots).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
    }

    #[test]
    fn roots_are_real_and_include_the_temporary_folder() {
        let workspace = tempfile::tempdir().unwrap();
        let missing = workspace.path().join("missing");
        let roots = upload_roots(&[
            workspace.path().to_path_buf(),
            workspace.path().to_path_buf(),
            missing,
        ]);
        let temp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(roots.iter().any(|r| *r == plain(temp.clone())));
        let real = plain(std::fs::canonicalize(workspace.path()).unwrap());
        assert_eq!(roots.iter().filter(|r| **r == real).count(), 1);
        assert!(roots.windows(2).all(|w| w[0] < w[1]));
    }
}
