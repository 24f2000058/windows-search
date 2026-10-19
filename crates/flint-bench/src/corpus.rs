//! Deterministic synthetic filesystem corpus resembling a Windows user volume:
//! deep trees, vendor/tooling directories, heavily duplicated names
//! (index.js, README.md), mixed case, digits, separators and ~5% non-ASCII.

use flint_fuzzy::table::{Meta, NameTable, NO_PARENT};

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
    pub fn chance(&mut self, pct: usize) -> bool {
        self.below(100) < pct
    }
}

const WORDS: &[&str] = &[
    "report", "invoice", "budget", "project", "draft", "final", "notes", "meeting", "summary", "config", "settings", "backup",
    "photo", "image", "video", "music", "track", "album", "holiday", "family", "scan", "contract", "letter", "resume", "data",
    "export", "import", "test", "spec", "design", "logo", "icon", "banner", "client", "server", "build", "release", "debug",
    "main", "index", "util", "helper", "module", "component", "service", "model", "view", "controller", "schema", "migration",
    "readme", "license", "changelog", "manual", "guide", "tutorial", "lecture", "thesis", "paper", "chapter", "presentation",
    "quarterly", "annual", "monthly", "weekly", "daily", "archive", "temp", "cache", "log", "error", "trace", "dump", "patch",
];
const UNICODE_WORDS: &[&str] = &["Résumé", "Übersicht", "Straße", "café", "naïve", "日本語", "写真", "メモ", "Отчёт", "документ", "Ελληνικά", "año"];
const DIRS: &[&str] = &[
    "Users", "AppData", "Local", "Roaming", "Microsoft", "Windows", "Program Files", "Documents", "Downloads", "Desktop",
    "Pictures", "Music", "Videos", "Projects", "src", "lib", "bin", "obj", "node_modules", "packages", ".git", "objects",
    "assets", "images", "docs", "tests", "build", "dist", "target", "release", "vendor", "Temp", "Cache", "Logs", "Work",
];
const EXTS: &[(&str, usize)] = &[
    ("txt", 8), ("pdf", 6), ("docx", 5), ("xlsx", 3), ("jpg", 10), ("png", 8), ("mp3", 3), ("mp4", 2), ("js", 12), ("ts", 6),
    ("json", 6), ("rs", 3), ("cpp", 3), ("h", 3), ("py", 4), ("dll", 5), ("exe", 2), ("log", 3), ("md", 3), ("zip", 2), ("", 2),
];
const COMMON: &[&str] = &["index.js", "README.md", "package.json", "LICENSE", "main.rs", "Makefile", "desktop.ini", "thumbs.db", ".gitignore"];

fn ext(r: &mut Rng) -> &'static str {
    let total: usize = EXTS.iter().map(|e| e.1).sum();
    let mut x = r.below(total);
    for (e, w) in EXTS {
        if x < *w {
            return e;
        }
        x -= w;
    }
    "txt"
}

pub fn file_name(r: &mut Rng) -> String {
    if r.chance(12) {
        return r.pick(COMMON).to_string();
    }
    let mut s = String::new();
    let parts = 1 + r.below(3);
    let style = r.below(4);
    for i in 0..parts {
        let w = if r.chance(5) { r.pick(UNICODE_WORDS).to_string() } else { r.pick(WORDS).to_string() };
        let w = match style {
            0 => w,
            1 => {
                let mut c = w.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
            2 => w.to_uppercase(),
            _ => w,
        };
        if i > 0 {
            s.push(*r.pick(&['_', '-', ' ', '.']));
        }
        s.push_str(&w);
    }
    if r.chance(40) {
        let sep = *r.pick(&["_", "-", " ", ""]);
        let max = if r.chance(50) { 100 } else { 100_000 };
        s.push_str(&format!("{sep}{}", r.below(max)));
    }
    let e = ext(r);
    if !e.is_empty() {
        s.push('.');
        s.push_str(e);
    }
    s
}

/// Builds a corpus with about `n` entries (directories ≈ 10%).
pub fn build(n: usize, seed: u64) -> NameTable {
    let mut r = Rng(seed);
    let mut t = NameTable::new();
    let mut dirs = vec![];
    for (v, root) in ["C:", "D:"].iter().enumerate() {
        dirs.push((t.push(NO_PARENT, root, Meta::dir(v as u16)), 0usize, v as u16));
    }
    let mut count = 2;
    while count < n {
        // Prefer recent directories to create deep, clustered trees.
        let idx = if r.chance(70) && dirs.len() > 50 { dirs.len() - 1 - r.below(50) } else { r.below(dirs.len()) };
        let (parent, depth, vol) = dirs[idx];
        if r.chance(10) && depth < 14 {
            let name = if r.chance(60) { r.pick(DIRS).to_string() } else { format!("{}{}", r.pick(WORDS), if r.chance(30) { format!("_{}", r.below(1000)) } else { String::new() }) };
            let id = t.push(parent, &name, Meta::dir(vol));
            dirs.push((id, depth + 1, vol));
        } else {
            let name = file_name(&mut r);
            t.push(parent, &name, Meta::file(vol, r.below(50_000_000) as u64, 1_400_000_000 + r.below(300_000_000) as i64));
        }
        count += 1;
    }
    t
}
