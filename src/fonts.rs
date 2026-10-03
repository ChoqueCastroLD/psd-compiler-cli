//! Font discovery and lookup by PostScript name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rustybuzz::ttf_parser;

const FONT_EXTENSIONS: [&str; 4] = ["ttf", "otf", "ttc", "otc"];
const FALLBACK_FAMILIES: [&str; 4] = ["dejavusans", "notosans", "liberationsans", "arial"];

struct Face {
    path: PathBuf,
    index: u32,
    postscript_name: String,
    family: String,
    data: OnceLock<Option<Vec<u8>>>,
}

#[derive(Clone)]
struct Names {
    index: u32,
    postscript: String,
    family: String,
    full: String,
}

type CacheKey = (PathBuf, u64, u64);

/// A set of font faces, looked up by the PostScript names Photoshop stores in type layers.
///
/// Faces added first win when two share a name, so add explicit font folders before system fonts.
/// Font files are only read when a face is actually used.
#[derive(Default)]
pub struct FontDb {
    faces: Vec<Face>,
    by_name: HashMap<String, usize>,
    cache_path: Option<PathBuf>,
    cache: HashMap<CacheKey, Vec<Names>>,
    cache_dirty: bool,
}

fn normalize(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

impl FontDb {
    /// Creates an empty database.
    pub fn new() -> FontDb {
        FontDb::default()
    }

    /// Creates an empty database that remembers font names in `path`, so later runs skip parsing
    /// unchanged files. Call [`FontDb::save_cache`] after adding fonts.
    pub fn with_cache(path: impl Into<PathBuf>) -> FontDb {
        let path = path.into();
        let mut db = FontDb::default();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                let [file, mtime, size, index, postscript, family, full] = f[..] else { continue };
                let (Ok(mtime), Ok(size), Ok(index)) = (mtime.parse(), size.parse(), index.parse()) else { continue };
                db.cache.entry((PathBuf::from(file), mtime, size)).or_default().push(Names {
                    index,
                    postscript: postscript.into(),
                    family: family.into(),
                    full: full.into(),
                });
            }
        }
        db.cache_path = Some(path);
        db
    }

    /// Default cache location: `$XDG_CACHE_HOME/psd-compiler/fonts.tsv` or `~/.cache/psd-compiler/fonts.tsv`.
    pub fn default_cache_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
        Some(base.join("psd-compiler").join("fonts.tsv"))
    }

    /// Writes the name cache if a cache path was set and new files were scanned.
    pub fn save_cache(&self) -> std::io::Result<()> {
        let Some(path) = self.cache_path.as_ref().filter(|_| self.cache_dirty) else { return Ok(()) };
        let mut out = String::new();
        for ((file, mtime, size), faces) in &self.cache {
            for n in faces {
                out += &format!(
                    "{}\t{mtime}\t{size}\t{}\t{}\t{}\t{}\n",
                    file.display(),
                    n.index,
                    n.postscript,
                    n.family,
                    n.full
                );
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, out)
    }

    /// Folders where the operating system and the current user keep fonts.
    pub fn system_font_dirs() -> Vec<PathBuf> {
        let mut dirs = vec![];
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            dirs.extend([home.join(".fonts"), home.join(".local/share/fonts"), home.join("Library/Fonts")]);
        }
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            dirs.push(PathBuf::from(data).join("fonts"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Microsoft\\Windows\\Fonts"));
        }
        if let Some(windir) = std::env::var_os("WINDIR") {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        }
        dirs.extend(
            ["/usr/share/fonts", "/usr/local/share/fonts", "/Library/Fonts", "/System/Library/Fonts"]
                .map(PathBuf::from),
        );
        dirs.dedup();
        dirs
    }

    /// Adds every font under the system and user font folders. Returns the number of faces added.
    pub fn add_system_fonts(&mut self) -> usize {
        Self::system_font_dirs().iter().map(|d| self.add_dir(d)).sum()
    }

    /// Adds every `.ttf`, `.otf`, `.ttc` and `.otc` file under `dir`, recursively.
    /// Returns the number of faces added; missing folders add nothing.
    pub fn add_dir(&mut self, dir: impl AsRef<Path>) -> usize {
        let mut seen = std::collections::HashSet::new();
        self.add_dir_walk(dir.as_ref(), &mut seen)
    }

    /// Walks `dir` at any depth; `seen` holds canonical folders already visited, so symlink loops
    /// end.
    fn add_dir_walk(&mut self, dir: &Path, seen: &mut std::collections::HashSet<PathBuf>) -> usize {
        let Ok(real) = std::fs::canonicalize(dir) else { return 0 };
        if !seen.insert(real) {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        let mut added = 0;
        for p in paths {
            if p.is_dir() {
                added += self.add_dir_walk(&p, seen);
            } else if has_font_extension(&p) {
                added += self.add_file(&p);
            }
        }
        added
    }

    /// Adds the faces of one font file. Returns the number of faces added.
    pub fn add_file(&mut self, path: impl AsRef<Path>) -> usize {
        let path = path.as_ref();
        let Ok(meta) = std::fs::metadata(path) else { return 0 };
        let mtime =
            meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let key = (path.to_path_buf(), mtime, meta.len());
        let names = match self.cache.get(&key) {
            Some(n) => n.clone(),
            None => {
                let n = scan(path);
                self.cache.insert(key, n.clone());
                self.cache_dirty = true;
                n
            }
        };
        let count = names.len();
        for n in names {
            self.push(path, n);
        }
        count
    }

    fn push(&mut self, path: &Path, n: Names) {
        let id = self.faces.len();
        for name in [&n.postscript, &n.full] {
            if !name.is_empty() {
                self.by_name.entry(normalize(name)).or_insert(id);
            }
        }
        self.faces.push(Face {
            path: path.to_path_buf(),
            index: n.index,
            postscript_name: n.postscript,
            family: n.family,
            data: OnceLock::new(),
        });
    }

    /// Number of faces.
    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// Whether no faces were added.
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// Whether a face matches `name` (PostScript or full name, case and punctuation insensitive).
    pub fn contains(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// File that provides `name`, if any.
    pub fn path_of(&self, name: &str) -> Option<&Path> {
        self.find(name).map(|i| self.faces[i].path.as_path())
    }

    pub(crate) fn find(&self, name: &str) -> Option<usize> {
        let key = normalize(name);
        if let Some(&i) = self.by_name.get(&key) {
            return Some(i);
        }
        let base = normalize(name.split('-').next().unwrap_or(""));
        if base.is_empty() {
            return None;
        }
        self.faces.iter().position(|f| normalize(&f.family) == base || normalize(&f.postscript_name) == base)
    }

    pub(crate) fn face(&self, i: usize) -> Option<rustybuzz::Face<'_>> {
        let f = &self.faces[i];
        let data = f.data.get_or_init(|| std::fs::read(&f.path).ok()).as_deref()?;
        rustybuzz::Face::from_slice(data, f.index)
    }

    pub(crate) fn has_glyph(&self, i: usize, c: char) -> bool {
        self.face(i).is_some_and(|f| f.glyph_index(c).is_some())
    }

    /// A face that covers `c`, preferring common sans-serif families.
    pub(crate) fn fallback_for(&self, c: char) -> Option<usize> {
        for family in FALLBACK_FAMILIES {
            let found = self
                .faces
                .iter()
                .position(|f| normalize(&f.postscript_name) == family || normalize(&f.family) == family);
            if let Some(i) = found.filter(|&i| self.has_glyph(i, c)) {
                return Some(i);
            }
        }
        (0..self.faces.len()).find(|&i| self.has_glyph(i, c))
    }
}

fn has_font_extension(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| FONT_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

fn scan(path: &Path) -> Vec<Names> {
    let Ok(data) = std::fs::read(path) else { return vec![] };
    let count = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
    (0..count)
        .filter_map(|index| {
            let face = ttf_parser::Face::parse(&data, index).ok()?;
            let mut n = Names { index, postscript: String::new(), family: String::new(), full: String::new() };
            for name in face.names() {
                let Some(s) = name.to_string() else { continue };
                let s = s.replace(['\t', '\n', '\r'], " ");
                let slot = match name.name_id {
                    ttf_parser::name_id::POST_SCRIPT_NAME => &mut n.postscript,
                    ttf_parser::name_id::FAMILY => &mut n.family,
                    ttf_parser::name_id::FULL_NAME => &mut n.full,
                    _ => continue,
                };
                if slot.is_empty() {
                    *slot = s;
                }
            }
            Some(n)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ps: &str, family: &str) -> Names {
        Names { index: 0, postscript: ps.into(), family: family.into(), full: format!("{family} Full") }
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize("Comic Sans-MS_Bold"), "comicsansmsbold");
    }

    #[test]
    fn finds_by_postscript_full_and_family_prefix() {
        let mut db = FontDb::new();
        db.push(Path::new("/a.ttf"), names("Anton-Regular", "Anton"));
        db.push(Path::new("/b.ttf"), names("WildWords", "Wild Words"));
        assert_eq!(db.find("anton-regular"), Some(0));
        assert_eq!(db.find("Wild Words Full"), Some(1));
        assert_eq!(db.find("Anton-Italic"), Some(0));
        assert_eq!(db.find("Missing-Bold"), None);
        assert_eq!(db.find(""), None);
        assert_eq!(db.len(), 2);
    }

    #[test]
    fn first_added_face_wins() {
        let mut db = FontDb::new();
        db.push(Path::new("/project/Anton.ttf"), names("Anton-Regular", "Anton"));
        db.push(Path::new("/usr/Anton.ttf"), names("Anton-Regular", "Anton"));
        assert_eq!(db.path_of("Anton-Regular"), Some(Path::new("/project/Anton.ttf")));
    }

    #[test]
    fn missing_dirs_and_files_add_nothing() {
        let mut db = FontDb::new();
        assert_eq!(db.add_dir("/definitely/not/here"), 0);
        assert_eq!(db.add_file("/definitely/not/here.ttf"), 0);
        assert!(db.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn walks_deep_folders_and_survives_symlink_loops() {
        let Some(font) = ["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/Library/Fonts/Arial.ttf"]
            .iter()
            .map(Path::new)
            .find(|p| p.exists())
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c/d/e/f/g/h/i");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::copy(font, deep.join("font.ttf")).unwrap();
        std::os::unix::fs::symlink(dir.path(), deep.join("loop")).unwrap();
        let mut db = FontDb::new();
        assert_eq!(db.add_dir(dir.path()), 1);
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("fonts.tsv");
        let font = dir.path().join("fake.ttf");
        std::fs::write(&font, b"not a font").unwrap();
        let mut db = FontDb::with_cache(&cache);
        assert_eq!(db.add_dir(dir.path()), 0);
        db.save_cache().unwrap();
        assert!(cache.exists());
        let db = FontDb::with_cache(&cache);
        assert_eq!(db.cache.len(), 0);
        assert!(has_font_extension(Path::new("x.OTF")));
        assert!(!has_font_extension(Path::new("x.woff")));
    }
}
