//! Offered text extensions, not a claim to enumerate every possible text format.
//! Pure data and bounded selection. No filesystem, desktop or worker access.
use std::path::Path;

pub const MAX_SELECTED_EXTENSIONS: usize = 256;
pub const MAX_EXTENSION_BYTES: usize = 64;
pub const ALL_SCOPE: &str = "All text types means every extension in Kwil's offered catalog. Custom extensions can be added individually. It does not mean every possible text extension.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    PlainText,
    Markup,
    Data,
    Configuration,
    Systems,
    Managed,
    Scripts,
    Web,
    Build,
    Scientific,
}
pub fn groups() -> &'static [Group] {
    &[
        Group::PlainText,
        Group::Markup,
        Group::Data,
        Group::Configuration,
        Group::Systems,
        Group::Managed,
        Group::Scripts,
        Group::Web,
        Group::Build,
        Group::Scientific,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub extension: &'static str,
    pub group: Group,
    pub common: bool,
}
macro_rules! entries {
    ($($group:ident, $common:literal => [$($ext:literal),* $(,)?]);* $(;)?) => {
        pub static CATALOG: &[CatalogEntry] = &[$($(CatalogEntry { extension: $ext, group: Group::$group, common: $common },)*)*];
    }
}
entries! {
    PlainText, true => ["txt", "text", "log"];
    PlainText, false => ["asc", "nfo", "todo"];
    Markup, true => ["md", "markdown", "rst"];
    Markup, false => ["mdown", "mkd", "mdx", "rmd", "qmd", "adoc", "asciidoc", "org", "tex", "bib", "sty", "cls", "pod"];
    Data, true => ["json", "jsonl", "ndjson", "xml", "csv", "tsv"];
    Data, false => ["jsonc", "json5", "geojson", "graphql", "gql", "xsd", "xsl", "xslt", "dtd", "rss", "atom", "svg", "plist", "srt", "vtt", "ics", "ttl", "proto", "thrift", "avsc"];
    Configuration, true => ["toml", "yaml", "yml", "ini", "cfg", "conf", "properties", "env", "editorconfig"];
    Configuration, false => ["config", "reg", "desktop", "service", "socket", "timer", "target", "mount", "rules", "gitignore", "gitattributes", "gitmodules", "npmrc", "dockerignore", "hcl", "tf", "tfvars", "nix", "ron", "kdl"];
    Systems, false => ["c", "cc", "cpp", "cxx", "h", "hh", "hpp", "hxx", "inl", "ipp", "tpp", "c++", "h++", "rs", "go", "zig", "d", "asm", "s", "inc", "f", "for", "f90", "f95", "f03", "f08", "ada", "adb", "ads", "pas", "pp", "vala"];
    Managed, false => ["java", "cs", "csx", "fs", "fsi", "fsx", "vb", "vbs", "kt", "kts", "scala", "groovy", "swift", "dart", "clj", "cljs", "cljc", "edn", "ex", "exs", "erl", "hrl", "hs", "ml", "mli", "lisp", "cl", "scm", "rkt", "elm", "purs"];
    Scripts, true => ["py", "sh", "ps1", "sql"];
    Scripts, false => ["pyi", "pyw", "rb", "rake", "gemspec", "pl", "pm", "lua", "bash", "zsh", "fish", "ksh", "csh", "tcsh", "psm1", "psd1", "bat", "cmd", "awk", "sed", "tcl", "r", "jl", "m", "php"];
    Web, true => ["html", "css", "js", "ts"];
    Web, false => ["htm", "xhtml", "scss", "sass", "less", "jsx", "tsx", "mjs", "cjs", "vue", "svelte", "astro", "ejs", "erb", "hbs", "handlebars", "mustache", "pug", "liquid", "jinja", "jinja2", "j2", "twig", "jsp", "aspx", "razor", "cshtml"];
    Build, false => ["mk", "mak", "cmake", "ninja", "gradle", "sbt", "bazel", "bzl", "gyp", "pro", "props", "targets", "csproj", "fsproj", "vbproj", "vcxproj", "sln", "slnx", "nuspec", "wxs", "wxi", "iss", "nsi", "dockerfile", "just"];
    Scientific, false => ["ipynb", "stan", "typ", "lean", "agda", "coq", "vhdl", "vhd", "sv", "svh", "glsl", "vert", "frag", "hlsl", "wgsl"];
}

pub fn catalog() -> &'static [CatalogEntry] {
    CATALOG
}
/// Filename-only hints are visible separately. They cannot be registered as
/// extensions: `Makefile` does not imply a `.Makefile` association.
pub fn filename_hints() -> &'static [&'static str] {
    &[
        "Makefile",
        "GNUmakefile",
        "Dockerfile",
        "Containerfile",
        "CMakeLists.txt",
        "Justfile",
        "Jenkinsfile",
        "Rakefile",
        "Gemfile",
        "BUILD",
        "WORKSPACE",
        ".env",
        ".gitignore",
        ".editorconfig",
    ]
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Extension(String);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionError {
    Invalid,
    TooMany,
    NonUnicode,
    NoExtension,
}
impl Extension {
    /// Registration uses case-insensitive ASCII keys on every backend, matching
    /// the existing association planner; language detection may differ on Unix.
    /// A leading dot is accepted, but paths/globs/lists/compound suffixes are not.
    pub fn parse(input: &str) -> Result<Self, ExtensionError> {
        let text = input.strip_prefix('.').unwrap_or(input);
        if text.is_empty()
            || text.len() > MAX_EXTENSION_BYTES
            || !text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+'))
        {
            return Err(ExtensionError::Invalid);
        }
        Ok(Self(text.to_ascii_lowercase()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
pub fn current_extension(path: &Path) -> Result<Extension, ExtensionError> {
    let extension = path.extension().ok_or(ExtensionError::NoExtension)?;
    Extension::parse(extension.to_str().ok_or(ExtensionError::NonUnicode)?)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    AllTextTypes,
    CommonTextConfig,
    PlainTextOnly,
    Custom,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    preset: Preset,
    extensions: Vec<Extension>,
}
impl Default for Selection {
    fn default() -> Self {
        Self::new(Preset::Custom)
    }
}
impl Selection {
    pub fn new(preset: Preset) -> Self {
        let mut extensions: Vec<_> = catalog()
            .iter()
            .filter(|entry| match preset {
                Preset::AllTextTypes => true,
                Preset::CommonTextConfig => entry.common,
                Preset::PlainTextOnly => matches!(entry.extension, "txt" | "text" | "log"),
                Preset::Custom => false,
            })
            .map(|entry| Extension::parse(entry.extension).expect("static catalog extension"))
            .collect();
        extensions.sort();
        extensions.dedup();
        assert!(extensions.len() <= MAX_SELECTED_EXTENSIONS, "catalog exceeds bounded selection");
        Self { preset, extensions }
    }
    pub fn preset(&self) -> Preset {
        self.preset
    }
    pub fn extensions(&self) -> &[Extension] {
        &self.extensions
    }
    pub fn contains(&self, extension: &Extension) -> bool {
        self.extensions.binary_search(extension).is_ok()
    }
    pub fn toggle(&mut self, extension: Extension, selected: bool) -> Result<(), ExtensionError> {
        match (self.extensions.binary_search(&extension), selected) {
            (Err(index), true) => {
                if self.extensions.len() == MAX_SELECTED_EXTENSIONS {
                    return Err(ExtensionError::TooMany);
                }
                self.extensions.insert(index, extension);
            }
            (Ok(index), false) => {
                self.extensions.remove(index);
            }
            _ => {}
        }
        self.preset = Preset::Custom;
        Ok(())
    }
    pub fn add_custom(&mut self, text: &str) -> Result<Extension, ExtensionError> {
        let extension = Extension::parse(text)?;
        self.toggle(extension.clone(), true)?;
        Ok(extension)
    }
}

#[cfg(test)]
#[path = "file_types_tests.rs"]
mod tests;
