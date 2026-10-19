//! "Programma's": the big data folders of installed programs (AI models,
//! emulators, Docker, build caches). Each one says how to clean it up
//! safely; only caches and model files can be removed from the app.

use crate::i18n::t;
use crate::junk::{self, CleanReport};
use crate::largefiles::LargeFile;
use std::path::{Path, PathBuf};

/// What the app may do with a location.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cleanup {
    /// Only explain how to clean it up in the program itself.
    Advice,
    /// A cache the program rebuilds by itself; its contents may be removed.
    Cache,
    /// Holds model files that may be moved to the Recycle Bin one by one.
    Models,
}

#[derive(Clone, Debug)]
pub struct Spot {
    pub title: String,
    pub advice: &'static str,
    pub cleanup: Cleanup,
    /// Existing folders that make up this location.
    pub folders: Vec<PathBuf>,
}

/// File types of AI models; only these may be removed from a model folder.
const MODEL_EXTENSIONS: [&str; 9] = ["safetensors", "ckpt", "pt", "pth", "bin", "gguf", "onnx", "sft", "ggml"];

/// Model files smaller than this are not listed.
pub const MIN_MODEL_SIZE: u64 = 50 << 20;

pub fn is_model_file(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| MODEL_EXTENSIONS.contains(&e.as_str()))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The known program locations that exist on this computer.
pub fn known_spots() -> Vec<Spot> {
    let (Some(home), Some(local), Some(roaming)) = (env_path("USERPROFILE"), env_path("LOCALAPPDATA"), env_path("APPDATA"))
    else {
        return Vec::new();
    };
    let packages = |prefix: &str| -> Vec<PathBuf> {
        std::fs::read_dir(local.join("Packages"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_name().to_string_lossy().to_lowercase().starts_with(prefix))
                    .map(|e| e.path())
                    .collect()
            })
            .unwrap_or_default()
    };
    let spot = |title: &str, advice: &'static str, cleanup: Cleanup, folders: Vec<PathBuf>| Spot {
        title: title.to_string(),
        advice,
        cleanup,
        folders: folders.into_iter().filter(|f| f.is_dir()).collect(),
    };
    let mut wsl = vec![local.join("wsl")];
    wsl.extend(packages("canonicalgroup"));
    let mut claude = vec![roaming.join("Claude")];
    claude.extend(packages("claude_"));

    vec![
        spot(
            t("ComfyUI-modellen", "ComfyUI models"),
            t(
                "Modellen die je niet meer gebruikt kun je weggooien; ComfyUI kan ze later opnieuw downloaden. \
                 Kies 'Modellen bekijken'.",
                "Models you no longer use can go; ComfyUI can download them again later. Choose 'View models'.",
            ),
            Cleanup::Models,
            vec![local.join("Comfy-Desktop")],
        ),
        spot(
            t("LM Studio-modellen", "LM Studio models"),
            t(
                "Verwijder modellen in LM Studio (My Models) of hier via 'Modellen bekijken'.",
                "Remove models in LM Studio (My Models) or here via 'View models'.",
            ),
            Cleanup::Models,
            vec![home.join(".lmstudio").join("models"), home.join(".cache").join("lm-studio")],
        ),
        spot(
            t("Ollama-modellen", "Ollama models"),
            t(
                "Ruim op via de opdrachtprompt: 'ollama list' toont je modellen, 'ollama rm <naam>' verwijdert er één. \
                 Niet met de hand verwijderen.",
                "Clean up from the command prompt: 'ollama list' shows your models, 'ollama rm <name>' removes one. \
                 Do not delete them by hand.",
            ),
            Cleanup::Advice,
            vec![home.join(".ollama")],
        ),
        spot(
            t("Hugging Face-modellen", "Hugging Face models"),
            t(
                "Ruim op met 'huggingface-cli delete-cache' (kiest per model wat weg mag).",
                "Clean up with 'huggingface-cli delete-cache' (lets you pick per model).",
            ),
            Cleanup::Advice,
            vec![home.join(".cache").join("huggingface")],
        ),
        spot(
            t("Android-emulators", "Android emulators"),
            t(
                "Verwijder emulators die je niet gebruikt in Android Studio: Device Manager → menu (drie puntjes) → Delete.",
                "Remove emulators you do not use in Android Studio: Device Manager → menu (three dots) → Delete.",
            ),
            Cleanup::Advice,
            vec![home.join(".android").join("avd")],
        ),
        spot(
            "Android SDK",
            t(
                "Verwijder oude SDK-versies en systeemimages in Android Studio: Settings → Android SDK.",
                "Remove old SDK versions and system images in Android Studio: Settings → Android SDK.",
            ),
            Cleanup::Advice,
            vec![local.join("Android").join("Sdk")],
        ),
        spot(
            "Docker",
            t(
                "Ruim op in Docker Desktop: Troubleshoot → Clean / Purge data, of 'docker system prune -a'.",
                "Clean up in Docker Desktop: Troubleshoot → Clean / Purge data, or 'docker system prune -a'.",
            ),
            Cleanup::Advice,
            vec![local.join("Docker")],
        ),
        spot(
            "WSL (Ubuntu)",
            t(
                "Dit is de schijf van je Linux-omgeving. Ruim bestanden op binnen Ubuntu; niet hier verwijderen.",
                "This is the disk of your Linux environment. Clean up files inside Ubuntu; do not delete it here.",
            ),
            Cleanup::Advice,
            wsl,
        ),
        spot(
            t("Claude-app", "Claude app"),
            t(
                "Bestanden van de Claude-app (o.a. de virtuele machine). Niet verwijderen.",
                "Files of the Claude app (including its virtual machine). Do not delete.",
            ),
            Cleanup::Advice,
            claude,
        ),
        spot(
            t("Gradle-cache", "Gradle cache"),
            t(
                "Downloads en builds van Gradle (Android). Mag leeg; wordt bij de volgende build opnieuw opgebouwd.",
                "Downloads and builds of Gradle (Android). Can be emptied; rebuilt on the next build.",
            ),
            Cleanup::Cache,
            vec![home.join(".gradle").join("caches")],
        ),
        spot(
            t("npm-cache", "npm cache"),
            t(
                "Gedownloade JavaScript-pakketten. Mag leeg; npm haalt ze opnieuw op als dat nodig is.",
                "Downloaded JavaScript packages. Can be emptied; npm fetches them again when needed.",
            ),
            Cleanup::Cache,
            vec![local.join("npm-cache")],
        ),
        spot(
            t("pip-cache", "pip cache"),
            t(
                "Gedownloade Python-pakketten. Mag leeg; pip haalt ze opnieuw op als dat nodig is.",
                "Downloaded Python packages. Can be emptied; pip fetches them again when needed.",
            ),
            Cleanup::Cache,
            vec![local.join("pip").join("Cache")],
        ),
        spot(
            t("NuGet-pakketten", "NuGet packages"),
            t(
                "Gedownloade .NET-pakketten. Mag leeg; ze worden bij de volgende build opnieuw opgehaald.",
                "Downloaded .NET packages. Can be emptied; they are fetched again on the next build.",
            ),
            Cleanup::Cache,
            vec![home.join(".nuget").join("packages")],
        ),
        spot(
            t("Cargo-register", "Cargo registry"),
            t(
                "Gedownloade Rust-crates. Mag leeg; cargo haalt ze opnieuw op bij de volgende build.",
                "Downloaded Rust crates. Can be emptied; cargo fetches them again on the next build.",
            ),
            Cleanup::Cache,
            vec![home.join(".cargo").join("registry")],
        ),
    ]
    .into_iter()
    .filter(|s| !s.folders.is_empty())
    .collect()
}

/// The remaining program folders in AppData (Local and Roaming), for
/// programs this app has no specific advice for.
pub fn other_program_folders(known: &[Spot]) -> Vec<Spot> {
    let roots: Vec<PathBuf> = ["LOCALAPPDATA", "APPDATA"].iter().filter_map(|n| env_path(n)).collect();
    let covered = |p: &Path| known.iter().flat_map(|s| &s.folders).any(|f| f.starts_with(p) || p.starts_with(f));
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let skip = !entry.file_type().is_ok_and(|k| k.is_dir() && !k.is_symlink())
                || covered(&path)
                || entry.file_name().eq_ignore_ascii_case("Temp");
            if !skip {
                out.push(Spot {
                    title: entry.file_name().to_string_lossy().into_owned(),
                    advice: t(
                        "Gegevens van een programma. Gebruik je het niet meer? Verwijder het programma via \
                         Instellingen → Apps.",
                        "Data of a program. No longer using it? Uninstall the program via Settings → Apps.",
                    ),
                    cleanup: Cleanup::Advice,
                    folders: vec![path],
                });
            }
        }
    }
    out
}

pub fn measure(spot: &Spot) -> u64 {
    junk::measure_folders(&spot.folders, None).bytes
}

/// The largest model files of a `Models` location.
pub fn model_files(spot: &Spot) -> Vec<LargeFile> {
    let mut files = Vec::new();
    let mut pending = spot.folders.clone();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                pending.push(path);
            } else if is_model_file(&path) {
                if let Some(meta) = entry.metadata().ok().filter(|m| m.len() >= MIN_MODEL_SIZE) {
                    files.push(LargeFile { size: meta.len(), path, modified: meta.modified().ok() });
                }
            }
        }
    }
    files.sort_by(|a, b| b.size.cmp(&a.size));
    files
}

/// Whether `path` may be removed: a model file inside one of the `Models`
/// locations in `spots`.
pub fn may_remove_model(spots: &[Spot], path: &Path) -> bool {
    is_model_file(path)
        && spots
            .iter()
            .filter(|s| s.cleanup == Cleanup::Models)
            .flat_map(|s| &s.folders)
            .any(|f| path.starts_with(f))
}

/// Empties a `Cache` location (the folders themselves stay).
pub fn clear_cache(spot: &Spot) -> CleanReport {
    if spot.cleanup != Cleanup::Cache {
        return CleanReport::default();
    }
    junk::clean_folders(&spot.folders, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("dr_apps_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, size: u64) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(&path).unwrap().set_len(size).unwrap();
            path
        }

        fn spot(&self, cleanup: Cleanup) -> Spot {
            Spot { title: "test".into(), advice: "", cleanup, folders: vec![self.0.clone()] }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recognises_model_files() {
        assert!(is_model_file(Path::new(r"C:\m\flux.SafeTensors")));
        assert!(is_model_file(Path::new(r"C:\m\qwen.gguf")));
        assert!(!is_model_file(Path::new(r"C:\m\python311.dll")));
        assert!(!is_model_file(Path::new(r"C:\m\config.json")));
    }

    #[test]
    fn lists_only_large_model_files_largest_first() {
        let t = TempTree::new("models");
        t.file("models/a.safetensors", MIN_MODEL_SIZE + 10);
        t.file("models/sub/b.gguf", MIN_MODEL_SIZE + 20);
        t.file("models/small.safetensors", 10);
        t.file("venv/torch.dll", MIN_MODEL_SIZE + 99);

        let files = model_files(&t.spot(Cleanup::Models));

        let names: Vec<String> = files.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["b.gguf", "a.safetensors"]);
    }

    #[test]
    fn only_models_in_a_model_location_may_be_removed() {
        let t = TempTree::new("refuse");
        let dll = t.file("venv/torch.dll", 10);
        let model = t.file("models/a.safetensors", 10);
        let elsewhere = PathBuf::from(r"C:\elders\a.safetensors");

        let models = [t.spot(Cleanup::Models)];
        let advice = [t.spot(Cleanup::Advice)];

        assert!(may_remove_model(&models, &model));
        assert!(!may_remove_model(&models, &dll));
        assert!(!may_remove_model(&models, &elsewhere));
        assert!(!may_remove_model(&advice, &model));
    }

    #[test]
    fn clears_caches_but_never_other_locations() {
        let t = TempTree::new("cache");
        let file = t.file("pkg/a.tgz", 100);

        assert_eq!(clear_cache(&t.spot(Cleanup::Advice)), CleanReport::default());
        assert!(file.exists());

        let report = clear_cache(&t.spot(Cleanup::Cache));
        assert_eq!((report.freed, file.exists(), t.0.is_dir()), (100, false, true));
    }
}
