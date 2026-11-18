//! Which files an agent may hand a page (`upload`): regular files inside
//! the thread's workspace or the temporary folder, judged by where they
//! really are (links followed), and not hard-linked from elsewhere.
//!
//! Which local files an agent below full access may open in its tab
//! (`navigate` to a `file:` address, [`check_open`]): the same kind of
//! file, inside the thread's workspace or its own captures folder only,
//! as Playwright MCP allows by default.

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

/// Where the local file `path` really is, if an agent below full access may
/// open it in its tab: within `roots` (canonical, as [`upload_roots`]
/// makes them, but without the temporary folder).
pub fn check_open(path: &Path, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let listed = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let shown = path.display();
    let outside = || {
        format!(
            "{shown} is outside the folders this thread may open local files from ({listed}). \
             Save the page in the workspace and open it from there; only a thread with full \
             access may open other local files."
        )
    };
    // A missing file says nothing about where it would be.
    let Ok(real) = std::fs::canonicalize(path).map(plain) else {
        return Err(outside());
    };
    if !roots.iter().any(|root| real.starts_with(root)) {
        return Err(outside());
    }
    let meta = std::fs::metadata(&real).map_err(|_| outside())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.is_file() && meta.nlink() > 1 {
            return Err(format!(
                "{shown} has other names elsewhere (hard links): copy it to a file of its own first"
            ));
        }
    }
    let _ = meta;
    Ok(real)
}

/// `roots` as they really are on disk (missing ones dropped).
pub fn real_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .map(plain)
        .collect()
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
    fn local_pages_open_from_the_workspace_only() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = real_roots(&[workspace.path().to_path_buf()]);
        let page = workspace.path().join("site/index.html");
        std::fs::create_dir_all(page.parent().unwrap()).unwrap();
        std::fs::write(&page, "<p>hi").unwrap();
        assert_eq!(
            check_open(&page, &roots).unwrap(),
            std::fs::canonicalize(&page).unwrap()
        );
        // A folder of it (a listing) too.
        check_open(&workspace.path().join("site"), &roots).unwrap();

        let secret = outside.path().join("id_ed25519");
        std::fs::write(&secret, "key").unwrap();
        let refused = check_open(&secret, &roots).unwrap_err();
        assert!(refused.contains("outside"), "{refused}");
        assert!(refused.contains("full access"), "{refused}");
        // Whether a file exists elsewhere isn't told.
        let missing = check_open(&outside.path().join("nope"), &roots).unwrap_err();
        assert!(missing.contains("outside"), "{missing}");
        assert!(check_open(Path::new("/etc/hosts"), &roots).is_err());
        // Up and out again.
        let dotted = workspace
            .path()
            .join("site/../../")
            .join(outside.path().file_name().unwrap());
        assert!(check_open(&dotted.join("id_ed25519"), &roots).is_err());

        #[cfg(unix)]
        {
            let link = workspace.path().join("key.html");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            assert!(check_open(&link, &roots).is_err());
            let hard = workspace.path().join("hard.html");
            std::fs::hard_link(&secret, &hard).unwrap();
            assert!(check_open(&hard, &roots).is_err());
        }
    }

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
