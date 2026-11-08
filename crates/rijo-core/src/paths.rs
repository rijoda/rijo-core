//! Wo Rijo Code seinen Zustand ablegt — die eine Quelle der Wahrheit.
//!
//! Vorher lasen 23 Stellen `std::env::var("HOME")` mit dem hartkodierten
//! Fallback `/home/user` (`guard.rs` sogar `/tmp`). Auf Windows ist HOME nicht
//! gesetzt (nur USERPROFILE), also landete dort alles unter
//! `C:\home\user\.rijo` — gemessen 2026-09-27. `dirs::home_dir()` war bereits
//! Dependency und wurde an genau einer Stelle verwendet (build_hint.rs);
//! dieses Modul macht es zur einzigen.
//!
//! Aufrufer bleiben plattformblind: niemand fragt noch `cfg!(windows)`, und
//! `embed.rs` / `router/mod.rs` nutzen nicht mehr den relativen Fallback `.`
//! — das war der §X-Bug (eine Modellkopie pro Arbeitsverzeichnis) in
//! Linux-Form, der auf Windows als Datei im Projektordner wiederkehrte.

use std::path::{Path, PathBuf};

/// Das Benutzerverzeichnis, plattformneutral.
///
/// `dirs::home_dir()` liest unter Windows `USERPROFILE`, unter Linux `HOME`.
/// Der Fallback ist `current_dir()` statt eines erfundenen Pfades: ein
/// relatives Ziel ist sichtbar und überschreibbar, `/home/user` auf dem
/// Windows-Laufwerk ist beides nicht.
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `~/.rijo` — Basis aller Zustandsverzeichnisse.
pub fn rijo_home() -> PathBuf {
    home_dir().join(".rijo")
}

/// `~/.hermes/skills` — die fremde Skill-Bibliothek, die Rijo Code mitliest.
pub fn hermes_skills_dir() -> PathBuf {
    home_dir().join(".hermes").join("skills")
}

/// `~/x/y` -> `<home>/x/y`. Alles andere unverändert.
pub fn expand_tilde(p: &Path) -> PathBuf {
    match p.to_str().and_then(|s| s.strip_prefix("~/")) {
        Some(rest) => home_dir().join(rest),
        None => p.to_path_buf(),
    }
}

/// Finds an executable on the PATH — what `which` does on Linux, without the
/// external process and without assuming `which` exists.
///
/// Windows has no `which`, so the two probes that used it returned nothing and
/// reported "not installed" for programs that were installed (Chromium, the TTS
/// backends). It also drops a process spawn per probe: a probe cost 2–5 ms
/// before, the search here is a few stats.
///
/// On Windows `PATHEXT` is honoured (`cmd` finds `cmd.exe`, `which` does not)
/// and the bare name is extended, because the tool tables carry bare names.
pub fn which_bin(name: &str) -> Option<PathBuf> {
    // A path (absolute or relative with a separator) is checked as given.
    if name.contains('/') || name.contains('\\') {
        let p = PathBuf::from(name);
        return p.is_file().then_some(p);
    }

    let path = std::env::var_os("PATH")?;

    // Only Windows resolves extensions this way; on Unix a bare name is the name.
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(&path) {
        let base = dir.join(name);
        if base.is_file() {
            return Some(base);
        }
        for ext in &exts {
            let mut with_ext = base.clone().into_os_string();
            with_ext.push(ext);
            let candidate = PathBuf::from(with_ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rijo_home_is_under_the_user_profile_not_a_hardcoded_unix_path() {
        let home = rijo_home();
        let joined = home.to_string_lossy().replace('\\', "/");
        assert!(
            !joined.starts_with("/home/user"),
            "hardcoded unix fallback leaked: {}",
            home.display()
        );
        assert!(
            joined.to_lowercase().contains(".rijo"),
            "rijo_home must end in .rijo: {}",
            home.display()
        );
    }

    #[test]
    fn every_home_path_starts_with_the_real_home() {
        // Die eigentliche Invariante: skills_dir, data_dir, config und
        // skill_roots liegen alle unterhalb von rijo_home(). Auf Linux war
        // das durch den gemeinsamen HOME-Wert implizit gegeben; nach der
        // Umstellung ist es eine Behauptung, die man pruefen kann.
        let cfg = crate::config::Config::default();
        for p in [
            &cfg.skills_dir,
            &cfg.data_dir,
            &crate::config::Config::default_path(),
        ] {
            assert!(
                p.starts_with(rijo_home()),
                "{} is not under {}",
                p.display(),
                rijo_home().display()
            );
        }
    }

    #[test]
    fn tilde_expands_to_the_real_home() {
        let expanded = expand_tilde(Path::new("~/x/y"));
        assert!(
            expanded.starts_with(home_dir()),
            "expanded={:?} home={:?}",
            expanded,
            home_dir()
        );
        assert_eq!(expanded, home_dir().join("x/y"));
    }

    /// The probe that replaced `which`: a program that is on the PATH must be
    /// found, and the name may come bare or with its extension.
    #[test]
    fn which_bin_finds_a_program_that_is_on_the_path() {
        // Present on every platform this runs on: cmd.exe on Windows, sh elsewhere.
        let probe = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(which_bin(probe).is_some(), "{probe} must be found on PATH");
    }

    #[test]
    fn which_bin_resolves_a_bare_name_on_windows() {
        // `cmd` must resolve to `cmd.exe` — the tool tables carry bare names,
        // and a bare name is exactly what `which` cannot do there.
        if cfg!(windows) {
            let found = which_bin("cmd").expect("cmd on PATH");
            let name = found.file_name().unwrap().to_string_lossy().to_lowercase();
            assert!(name.starts_with("cmd."), "expected cmd.<ext>, got {name}");
        }
    }

    #[test]
    fn which_bin_returns_none_for_a_program_that_does_not_exist() {
        assert!(which_bin("rijo-nonexistent-probe-xyz").is_none());
    }

    #[test]
    fn which_bin_accepts_a_path_with_a_separator() {
        // A configured absolute path is checked as given, not searched.
        let current = std::env::current_exe().expect("current exe");
        assert_eq!(which_bin(&current.to_string_lossy()), Some(current.clone()));
        assert!(which_bin("/definitely/not/here/xyz").is_none());
    }
}
