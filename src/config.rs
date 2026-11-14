//! Where a run's profile comes from: `--profile FILE`, `--preset NAME`, or
//! else the settings file.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dps::Profile;

use crate::presets::Preset;

/// The environment variables that locate the settings file.
pub struct Dirs {
    pub xdg_config_home: Option<OsString>,
    pub home: Option<OsString>,
}

impl Dirs {
    pub fn from_env() -> Self {
        Self {
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
            home: std::env::var_os("HOME"),
        }
    }

    /// `$XDG_CONFIG_HOME/dps/profile.toml`, or `~/.config/dps/profile.toml`
    /// when that variable is unset, empty or relative (which the XDG spec
    /// says to ignore).
    pub fn settings_file(&self) -> Result<PathBuf, String> {
        let config = match &self.xdg_config_home {
            Some(xdg) if Path::new(xdg).is_absolute() => PathBuf::from(xdg),
            _ => self
                .home()
                .map_err(|e| format!("cannot locate the settings file: {e}"))?
                .join(".config"),
        };
        Ok(config.join("dps").join("profile.toml"))
    }

    /// The home directory, where the settings file and the background
    /// service's files live. A relative HOME is refused: launchd starts the
    /// service in /, where it would name other files than here.
    pub fn home(&self) -> Result<PathBuf, String> {
        match &self.home {
            Some(home) if Path::new(home).is_absolute() => Ok(PathBuf::from(home)),
            Some(home) if !home.is_empty() => Err(format!(
                "HOME is {}, which is not an absolute path; set HOME to your home directory",
                Path::new(home).display()
            )),
            _ => Err("HOME is not set; set HOME to your home directory".into()),
        }
    }
}

pub enum Source {
    File(PathBuf),
    Preset(&'static Preset),
    Settings(PathBuf),
}

impl Source {
    /// clap rejects `profile` and `preset` together.
    pub fn new(
        profile: Option<PathBuf>,
        preset: Option<&'static Preset>,
        dirs: &Dirs,
    ) -> Result<Self, String> {
        Ok(match (profile, preset) {
            (Some(path), _) => Self::File(path),
            (None, Some(preset)) => Self::Preset(preset),
            (None, None) => Self::Settings(dirs.settings_file()?),
        })
    }

    /// The arguments that select this source again, with a profile file made
    /// absolute so they work from any directory.
    #[cfg(target_os = "macos")]
    pub fn args(&self) -> Result<Vec<String>, String> {
        Ok(match self {
            Self::File(path) => {
                let path = fs::canonicalize(path)
                    .map_err(|e| format!("cannot read profile {}: {e}", path.display()))?;
                let path = path
                    .to_str()
                    .ok_or_else(|| format!("{} is not a UTF-8 path", path.display()))?;
                vec!["--profile".into(), path.into()]
            }
            Self::Preset(preset) => vec!["--preset".into(), preset.name.into()],
            Self::Settings(_) => Vec::new(),
        })
    }

    /// What messages call the profile: its file, or e.g. "preset music".
    pub fn name(&self) -> String {
        match self {
            Self::File(path) | Self::Settings(path) => path.display().to_string(),
            Self::Preset(preset) => format!("preset {}", preset.name),
        }
    }

    /// Reads the profile, first creating the settings file with the default
    /// profile if it is the source and does not exist yet.
    pub fn load(&self) -> Result<Profile, String> {
        match self {
            Self::File(path) => load_profile(path),
            Self::Preset(preset) => Ok(preset.profile()),
            Self::Settings(path) => {
                if create_default(path)? {
                    eprintln!(
                        "created settings file {} with the default profile",
                        path.display()
                    );
                }
                load_profile(path)
            }
        }
    }
}

pub fn default_toml() -> String {
    toml::to_string(&Profile::default()).expect("a profile always serializes")
}

/// Writes the default profile to `path` unless something is already there,
/// and says whether it did.
fn create_default(path: &Path) -> Result<bool, String> {
    let cannot = |e: io::Error| format!("cannot create settings file {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(cannot)?;
    }
    // create_new also settles a race with another dps creating it.
    let mut file = match File::create_new(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(cannot(e)),
    };
    if let Err(e) = file.write_all(default_toml().as_bytes()) {
        let _ = fs::remove_file(path);
        return Err(cannot(e));
    }
    Ok(true)
}

pub fn load_profile(path: &Path) -> Result<Profile, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read profile {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| {
        // Quoting the source line names the key, which type errors leave out.
        let location = e
            .span()
            .map(|span| {
                let line = text[..span.start].matches('\n').count();
                let source = text.lines().nth(line).unwrap_or_default().trim();
                format!(":{}: `{source}`", line + 1)
            })
            .unwrap_or_default();
        let message = e.message().trim().replace('\n', "; ");
        format!("{}{location}: {message}", path.display())
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::presets;

    /// A directory under the system temp dir, removed on drop.
    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new(test: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dps-unit-{}-{test}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn dirs(&self) -> Dirs {
            Dirs {
                xdg_config_home: Some(self.0.join("xdg").into()),
                home: Some(self.0.join("home").into()),
            }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn dirs(xdg: Option<&str>, home: Option<&str>) -> Dirs {
        Dirs {
            xdg_config_home: xdg.map(OsString::from),
            home: home.map(OsString::from),
        }
    }

    #[test]
    fn settings_file_prefers_an_absolute_xdg_config_home() {
        let file = |d: Dirs| d.settings_file().map(|p| p.display().to_string());
        assert_eq!(
            file(dirs(Some("/x"), Some("/h"))).unwrap(),
            "/x/dps/profile.toml"
        );
        // The XDG spec says to ignore a relative path.
        for ignored in ["", "x", "./x", "x/../y"] {
            assert_eq!(
                file(dirs(Some(ignored), Some("/h"))).unwrap(),
                "/h/.config/dps/profile.toml",
                "{ignored:?}"
            );
        }
        assert_eq!(
            file(dirs(None, Some("/h"))).unwrap(),
            "/h/.config/dps/profile.toml"
        );
        assert_eq!(file(dirs(Some("/x"), None)).unwrap(), "/x/dps/profile.toml");
        for d in [
            dirs(None, None),
            dirs(Some(""), Some("")),
            dirs(Some("x"), None),
        ] {
            let e = file(d).unwrap_err();
            assert_eq!(
                e,
                "cannot locate the settings file: HOME is not set; set HOME to your home directory"
            );
        }
        // A relative HOME would name other files for the background service,
        // which launchd starts in /.
        for d in [dirs(None, Some("h")), dirs(Some("x"), Some("./h"))] {
            let e = file(d).unwrap_err();
            assert!(e.contains("not an absolute path"), "{e}");
        }
        assert!(dirs(None, Some("h")).home().is_err());
        assert_eq!(dirs(None, Some("/h")).home().unwrap(), Path::new("/h"));
    }

    #[test]
    fn profile_file_wins_then_preset_then_settings() {
        let d = dirs(Some("/x"), None);
        let music = presets::find("music").unwrap();
        let s = Source::new(Some("a.toml".into()), Some(music), &d).unwrap();
        assert!(matches!(&s, Source::File(p) if p == Path::new("a.toml")));
        assert_eq!(s.name(), "a.toml");
        let s = Source::new(None, Some(music), &d).unwrap();
        assert!(matches!(s, Source::Preset(p) if p.name == "music"));
        assert_eq!(s.name(), "preset music");
        let s = Source::new(None, None, &d).unwrap();
        assert!(matches!(&s, Source::Settings(p) if p == Path::new("/x/dps/profile.toml")));
        // Only the settings file needs the variables.
        assert!(Source::new(Some("a.toml".into()), None, &dirs(None, None)).is_ok());
        assert!(Source::new(None, Some(music), &dirs(None, None)).is_ok());
        assert!(Source::new(None, None, &dirs(None, None)).is_err());
    }

    #[test]
    fn settings_file_is_created_once_with_the_default_profile() {
        let dir = TestDir::new("create-settings");
        let path = dir.dirs().settings_file().unwrap();
        let source = Source::Settings(path.clone());
        assert_eq!(source.load().unwrap(), Profile::default());
        assert_eq!(fs::read_to_string(&path).unwrap(), default_toml());

        fs::write(&path, "output_gain_db = -3.0\n").unwrap();
        assert!(!create_default(&path).unwrap());
        assert_eq!(source.load().unwrap().output_gain_db, -3.0);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "output_gain_db = -3.0\n"
        );
    }
}
