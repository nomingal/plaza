pub mod apt;
pub mod aur;
pub mod dnf;
pub mod flatpak;
pub mod installed;
pub mod pacman;
pub mod updates;

use crate::model::{Action, CommandLine, PackageHit, SourceId};
use async_trait::async_trait;

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    fn display_name(&self) -> &'static str;
    async fn search(&self, query: &str) -> anyhow::Result<Vec<PackageHit>>;
    fn action_command(&self, action: Action, pkg: &str) -> CommandLine;

    /// Optional hook to preload/refresh source state in the background. Called
    /// once at startup and again after an install/upgrade completes. Default is a
    /// no-op; dnf overrides it to build its in-memory catalog. Must be cheap to
    /// call (spawns its own task) and safe to call repeatedly.
    fn warm(&self) {}
}

/// Recursive size of everything under `path`, optionally only files with
/// extension `ext`. `None` when the root is missing or unreadable (the UI
/// shows a dash); unreadable entries below the root are skipped, not fatal.
pub fn dir_size(path: &std::path::Path, ext: Option<&str>) -> Option<u64> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut total = 0u64;
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&p, ext).unwrap_or(0);
        } else if ext.is_none_or(|e| p.extension().and_then(|x| x.to_str()) == Some(e)) {
            total += meta.len();
        }
    }
    Some(total)
}

/// The AUR helper's build-cache directory (`~/.cache/yay`, paru's clone dir).
pub fn aur_cache_dir(aur_bin: &str) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let cache = std::path::PathBuf::from(home).join(".cache");
    match aur_bin {
        "yay" => Some(cache.join("yay")),
        "paru" => Some(cache.join("paru").join("clone")),
        _ => None,
    }
}

/// Return true if `bin` is an executable found on `$PATH`.
pub fn which(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':')
        .map(|dir| std::path::Path::new(dir).join(bin))
        .any(|p| p.is_file())
}

/// True when Flatpak is usable: the binary is on PATH AND at least one remote is
/// configured. A binary with no remotes is a distinct state (the UI shows a
/// "no remotes configured" hint) since searching would return nothing.
pub fn has_usable_flatpak() -> bool {
    if !which("flatpak") {
        return false;
    }
    match std::process::Command::new("flatpak")
        .env("LC_ALL", "C")
        .arg("remotes")
        .output()
    {
        Ok(out) => !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Drop any source whose id is in `disabled`. Pure, so it is unit-tested apart
/// from the `which`/`flatpak` probing. Disabling every source is allowed.
pub fn filter_enabled(ids: &[SourceId], disabled: &[SourceId]) -> Vec<SourceId> {
    ids.iter().copied().filter(|id| !disabled.contains(id)).collect()
}

/// Build the set of enabled sources based on which binaries are installed,
/// minus any the user disabled. Any source can be turned off, including all of
/// them.
pub fn detect_sources(disabled: &[SourceId]) -> Vec<Box<dyn Source>> {
    let mut sources: Vec<Box<dyn Source>> = Vec::new();
    if which("pacman") && !disabled.contains(&SourceId::Pacman) {
        sources.push(Box::new(pacman::PacmanSource::new()));
    }
    if which("pacman") && !disabled.contains(&SourceId::Aur) {
        // AUR search is a plain RPC call and needs no local helper, so it shows
        // whenever we are on an Arch-like system (pacman present). Install and
        // upgrade are gated on a helper binary (yay/paru) separately.
        sources.push(Box::new(aur::AurSource::new()));
    }
    if has_usable_flatpak() && !disabled.contains(&SourceId::Flatpak) {
        sources.push(Box::new(flatpak::FlatpakSource::new()));
    }
    if which("apt-get") && !disabled.contains(&SourceId::Apt) {
        sources.push(Box::new(apt::AptSource::new()));
    }
    if which("dnf") && !disabled.contains(&SourceId::Dnf) {
        sources.push(Box::new(dnf::DnfSource::new()));
    }
    sources
}

/// Detect which AUR helper binaries are present on PATH, as `(yay, paru)`.
pub fn detect_aur_helpers() -> (bool, bool) {
    (which("yay"), which("paru"))
}

#[cfg(test)]
pub struct MockSource {
    pub id: SourceId,
    pub hits: Vec<PackageHit>,
}

#[cfg(test)]
#[async_trait]
impl Source for MockSource {
    fn id(&self) -> SourceId {
        self.id
    }
    fn display_name(&self) -> &'static str {
        "mock"
    }
    async fn search(&self, _query: &str) -> anyhow::Result<Vec<PackageHit>> {
        Ok(self.hits.clone())
    }
    fn action_command(&self, _action: Action, pkg: &str) -> CommandLine {
        CommandLine {
            program: "true".into(),
            args: vec![pkg.into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_a_ubiquitous_binary() {
        assert!(which("sh"));
        assert!(!which("definitely-not-a-real-binary-xyz"));
    }

    #[test]
    fn filter_enabled_drops_disabled_including_all() {
        let all = [SourceId::Pacman, SourceId::Aur, SourceId::Flatpak];
        assert_eq!(
            filter_enabled(&all, &[SourceId::Aur]),
            vec![SourceId::Pacman, SourceId::Flatpak]
        );
        assert!(filter_enabled(&all, &all).is_empty()); // all disabled allowed
        assert_eq!(filter_enabled(&all, &[]), all.to_vec());
    }

    #[tokio::test]
    async fn mock_source_returns_its_hits() {
        let src = MockSource {
            id: SourceId::Aur,
            hits: vec![PackageHit {
                name: "x".into(),
                version: "1".into(),
                source_id: SourceId::Aur,
                description: String::new(),
                meta: Default::default(),
            }],
        };
        let hits = src.search("anything").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(src.id(), SourceId::Aur);
    }

    #[test]
    fn dir_size_sums_recursively_and_filters_ext() {
        let root = std::env::temp_dir().join(format!("plaza-dirsize-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("a.pkg"), vec![0u8; 100]).unwrap();
        std::fs::write(sub.join("b.rpm"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size(&root, None), Some(150));
        assert_eq!(dir_size(&root, Some("rpm")), Some(50));
        assert_eq!(dir_size(std::path::Path::new("/definitely/not/here"), None), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn aur_cache_dir_maps_helper_to_path() {
        std::env::var("HOME").expect("HOME set in tests");
        assert!(aur_cache_dir("yay").unwrap().ends_with(".cache/yay"));
        assert!(aur_cache_dir("paru").unwrap().ends_with(".cache/paru/clone"));
        assert!(aur_cache_dir("").is_none());
    }
}
