//! File access is limited to folders the user allowed when starting the
//! server. Every path in a request is resolved (relative paths against the
//! first allowed folder) and checked before the request runs.

use std::path::{Component, Path, PathBuf};

use kerf_core::ops::{Request, Version};

#[derive(Clone, Debug)]
pub struct Sandbox {
    roots: Vec<PathBuf>,
}

impl Sandbox {
    /// Allowed folders; each must exist.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> std::io::Result<Self> {
        let roots = roots
            .into_iter()
            .map(|r| std::fs::canonicalize(&r))
            .collect::<std::io::Result<Vec<_>>>()?;
        if roots.is_empty() {
            return Err(std::io::Error::other("no allowed folders"));
        }
        Ok(Self { roots })
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The absolute path for `p`, if it lies inside an allowed folder.
    /// Paths that don't exist yet (export targets) are checked through their
    /// nearest existing parent, so symlinks can't lead outside.
    pub fn resolve(&self, p: &Path) -> Result<PathBuf, String> {
        let joined = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.roots[0].join(p)
        };
        // Split into the longest existing prefix and the rest.
        let mut existing = joined.clone();
        let mut rest = Vec::new();
        while !existing.exists() {
            match (existing.file_name(), existing.parent()) {
                (Some(name), Some(parent)) => {
                    rest.push(name.to_os_string());
                    existing = parent.to_path_buf();
                }
                _ => break,
            }
        }
        let mut resolved =
            std::fs::canonicalize(&existing).map_err(|e| format!("{}: {e}", existing.display()))?;
        for part in rest.iter().rev() {
            if Path::new(part)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(format!("{} is not a plain path", p.display()));
            }
            resolved.push(part);
        }
        if self.roots.iter().any(|r| resolved.starts_with(r)) {
            Ok(resolved)
        } else {
            Err(format!(
                "{} is outside the folders this server may use ({})",
                p.display(),
                self.roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }

    /// Resolve and check every path in a request.
    pub fn check(&self, mut req: Request) -> Result<Request, String> {
        let paths: Vec<&mut PathBuf> = match &mut req {
            Request::Open { path, .. } | Request::Import { path, .. } => vec![path],
            Request::Save { path, .. } => path.iter_mut().collect(),
            Request::Export { path, .. } => vec![path],
            Request::Compare { a, b, .. } => [a, b]
                .into_iter()
                .filter_map(|v| match v {
                    Version::File(p) => Some(p),
                    _ => None,
                })
                .collect(),
            Request::SetAlbum { cover, .. } => cover
                .iter_mut()
                .filter(|c| !c.as_os_str().is_empty())
                .collect(),
            _ => Vec::new(),
        };
        for p in paths {
            *p = self.resolve(p)?;
        }
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_allowed_folders() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir_all(music.join("show")).unwrap();
        std::fs::write(music.join("show/a.wav"), b"").unwrap();
        let sb = Sandbox::new([music.clone()]).unwrap();
        let music = std::fs::canonicalize(&music).unwrap();

        assert_eq!(
            sb.resolve(Path::new("show/a.wav")).unwrap(),
            music.join("show/a.wav")
        );
        assert_eq!(
            sb.resolve(&music.join("out/new/x.flac")).unwrap(),
            music.join("out/new/x.flac"),
            "new files and folders are fine"
        );
        assert!(sb.resolve(Path::new("../secret.txt")).is_err());
        assert!(sb.resolve(Path::new("/etc/passwd")).is_err());
        assert!(sb.resolve(Path::new("show/../../x")).is_err());
        assert!(sb.resolve(Path::new("nope/../../x")).is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", music.join("link")).unwrap();
            assert!(
                sb.resolve(Path::new("link/passwd")).is_err(),
                "symlinks out are caught"
            );
        }

        let req = sb
            .check(Request::Open {
                path: "show/a.wav".into(),
                discard: false,
            })
            .unwrap();
        assert_eq!(
            req,
            Request::Open {
                path: music.join("show/a.wav"),
                discard: false
            }
        );
        assert!(sb
            .check(Request::Save {
                path: Some("/tmp/../etc/x.kerf".into()),
                force: true
            })
            .is_err());
        assert!(sb.check(Request::Undo).is_ok());

        let compare = |b: &str| Request::Compare {
            a: Version::History(0),
            b: Version::File(b.into()),
            start: None,
            end: None,
            tracks: None,
            align: false,
            max_offset_secs: None,
            threshold_db: None,
        };
        let Request::Compare { a, b, .. } = sb.check(compare("show/a.wav")).unwrap() else {
            unreachable!()
        };
        assert_eq!(a, Version::History(0));
        assert_eq!(b, Version::File(music.join("show/a.wav")));
        assert!(
            sb.check(compare("/etc/passwd")).is_err(),
            "files to compare stay inside too"
        );
    }
}
