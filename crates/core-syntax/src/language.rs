//! Language metadata stays available even when a grammar feature is disabled.
use crate::Grammar;

macro_rules! languages {
    ($(($variant:ident, $id:literal, $feature:literal, $module:ident, $ext:literal, $names:literal, $keywords:literal, $line:literal, $open:literal, $close:literal)),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Language { Generic, $($variant),* }
        impl Language {
            /// The audited 32 grammar identities. Generic has no grammar.
            pub const ALL: &'static [Self] = &[$(Self::$variant),*];
            /// Typed picker catalogue, independent of enabled grammar features.
            pub const CATALOGUE: &'static [Self] = &[Self::Generic, $(Self::$variant),*];
            pub fn id(self) -> &'static str { match self { Self::Generic => "generic", $(Self::$variant => $id),* } }
            pub(crate) fn lexical(self) -> Lexical { match self { Self::Generic => Lexical { keywords: "", line: "", open: "/*", close: "*/" }, $(Self::$variant => Lexical { keywords: $keywords, line: $line, open: $open, close: $close }),* } }
            pub fn extensions(self) -> &'static str { match self { Self::Generic => "", $(Self::$variant => $ext),* } }
            pub fn filenames(self) -> &'static str { match self { Self::Generic => "", $(Self::$variant => $names),* } }
            pub fn bundled(self) -> Option<Grammar> { match self { $(
                #[cfg(feature = $feature)]
                Self::$variant => Some(Grammar { language: $module::language().into(), highlights: bundled_query(self, &$module::HIGHLIGHTS_QUERY) }),
            )* #[allow(unreachable_patterns)] _ => None } }
        }
    }
}

// Use the official grammar: the Arborium orchard fork's pinned source is unavailable.
#[cfg(feature = "lang-rust")]
mod bundled_rust {
    pub fn language() -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }
    pub const HIGHLIGHTS_QUERY: &str = tree_sitter_rust::HIGHLIGHTS_QUERY;
}

fn bundled_query(language: Language, query: &'static str) -> std::borrow::Cow<'static, str> {
    if language == Language::Sql {
        // Arborium SQL 2.18.2 has exactly two Lua %d predicates. Query::new uses
        // Rust regex syntax. Patch only these audited expressions, not arbitrary
        // query text or provider queries (where '%' may be a literal).
        std::borrow::Cow::Owned(
            query
                .replace(r#""^[-+]?%d+$""#, r#""^[-+]?[0-9]+$""#)
                .replace(r#""^[-+]?%d*\.%d*$""#, r#""^[-+]?[0-9]*\\.[0-9]*$""#),
        )
    } else {
        std::borrow::Cow::Borrowed(query)
    }
}

pub(crate) struct Lexical {
    pub keywords: &'static str,
    pub line: &'static str,
    pub open: &'static str,
    pub close: &'static str,
}

languages! {
    (C, "c", "lang-c", arborium_c, "c h", "", "auto break case char const continue default do double else enum extern float for goto if inline int long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while", "//", "/*", "*/"),
    (Cpp, "cpp", "lang-cpp", arborium_cpp, "cc cpp cxx hh hpp hxx", "", "alignas alignof asm auto bool break case catch char class const constexpr continue decltype default delete do double else enum explicit export extern false float for friend if inline int long namespace new noexcept nullptr operator private protected public return short signed sizeof static struct switch template this throw true try typedef typename union unsigned using virtual void volatile while", "//", "/*", "*/"),
    (CSharp, "c-sharp", "lang-c-sharp", arborium_c_sharp, "cs", "", "abstract as async await base bool break byte case catch char checked class const continue decimal default delegate do double else enum event explicit extern false finally fixed float for foreach if implicit in int interface internal is lock long namespace new null object operator out override params private protected public readonly ref return sbyte sealed short sizeof stackalloc static string struct switch this throw true try typeof uint ulong unchecked unsafe ushort using var virtual void volatile while yield", "//", "/*", "*/"),
    (Css, "css", "lang-css", arborium_css, "css", "", "important inherit initial unset auto none", "", "/*", "*/"),
    (Go, "go", "lang-go", arborium_go, "go", "", "break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var", "//", "/*", "*/"),
    (Html, "html", "lang-html", arborium_html, "html htm", "", "DOCTYPE", "", "<!--", "-->"),
    (Java, "java", "lang-java", arborium_java, "java", "", "abstract assert boolean break byte case catch char class const continue default do double else enum extends final finally float for if implements import instanceof int interface long native new null package private protected public return short static strictfp super switch synchronized this throw throws transient try void volatile while true false", "//", "/*", "*/"),
    (JavaScript, "javascript", "lang-javascript", arborium_javascript, "js mjs cjs", "", "async await break case catch class const continue debugger default delete do else export extends false finally for function if import in instanceof let new null of return static super switch this throw true try typeof undefined var void while with yield", "//", "/*", "*/"),
    (Json, "json", "lang-json", arborium_json, "json jsonc", "", "true false null", "//", "/*", "*/"),
    (Kotlin, "kotlin", "lang-kotlin", arborium_kotlin, "kt kts", "", "as break class continue do else false for fun if in interface is null object package return super this throw true try typealias typeof val var when while by catch constructor delegate dynamic field file finally get import init param property receiver set setparam where actual abstract annotation companion const crossinline data enum expect external final infix inline inner internal lateinit noinline open operator out override private protected public reified sealed suspend tailrec vararg", "//", "/*", "*/"),
    (Lua, "lua", "lang-lua", arborium_lua, "lua", "", "and break do else elseif end false for function goto if in local nil not or repeat return then true until while", "--", "--[[", "]]"),
    (Markdown, "markdown", "lang-markdown", arborium_markdown, "md markdown mdown", "", "", "", "<!--", "-->"),
    (Php, "php", "lang-php", arborium_php, "php phtml", "", "abstract and array as break callable case catch class clone const continue declare default die do echo else elseif empty enddeclare endfor endforeach endif endswitch endwhile enum eval exit extends final finally fn for foreach function global goto if implements include include_once instanceof insteadof interface isset list match namespace new null or print private protected public readonly require require_once return static switch throw trait try unset use var while xor yield true false", "//", "/*", "*/"),
    (Python, "python", "lang-python", arborium_python, "py pyi pyw", "", "False None True and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield match case", "#", "\"\"\"", "\"\"\""),
    (Ruby, "ruby", "lang-ruby", arborium_ruby, "rb rake gemspec", "Gemfile Rakefile", "BEGIN END alias and begin break case class def defined do else elsif end ensure false for if in module next nil not or redo rescue retry return self super then true undef unless until when while yield", "#", "=begin", "=end"),
    (Rust, "rust", "lang-rust", bundled_rust, "rs", "", "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while", "//", "/*", "*/"),
    (Bash, "bash", "lang-bash", arborium_bash, "sh bash zsh", "", "if then else elif fi case esac for select while until do done in function time coproc", "#", "", ""),
    (Sql, "sql", "lang-sql", arborium_sql, "sql", "", "select from where join inner outer left right full cross on as insert into values update set delete create alter drop table view index database primary key foreign references not null default unique check and or in is like between exists union all distinct group by having order asc desc limit offset with recursive begin end case when then else commit rollback transaction grant revoke true false", "--", "/*", "*/"),
    (Swift, "swift", "lang-swift", arborium_swift, "swift", "", "associatedtype class deinit enum extension fileprivate func import init inout internal let open operator private protocol public rethrows static struct subscript typealias var break case continue default defer do else fallthrough for guard if in repeat return switch where while as Any catch false is nil super self Self throw throws true try async await", "//", "/*", "*/"),
    (Toml, "toml", "lang-toml", arborium_toml, "toml", "Cargo.lock", "true false", "#", "\"\"\"", "\"\"\""),
    (TypeScript, "typescript", "lang-typescript", arborium_typescript, "ts mts cts", "", "abstract any as asserts async await boolean break case catch class const constructor continue debugger declare default delete do else enum export extends false finally for from function get if implements import in infer instanceof interface is keyof let module namespace never new null number object of private protected public readonly require return set static string super switch symbol this throw true try type typeof undefined unique unknown var void while with yield", "//", "/*", "*/"),
    (Tsx, "tsx", "lang-tsx", arborium_tsx, "tsx jsx", "", "async await break case catch class const continue default else export extends false finally for function if import in interface let new null return static super switch this throw true try type typeof undefined var void while yield", "//", "/*", "*/"),
    (Xml, "xml", "lang-xml", arborium_xml, "xml xsl xslt svg xsd", "", "DOCTYPE", "", "<!--", "-->"),
    (Yaml, "yaml", "lang-yaml", arborium_yaml, "yaml yml", "", "true false null yes no on off", "#", "", ""),
    (Dockerfile, "dockerfile", "lang-dockerfile", arborium_dockerfile, "dockerfile", "Dockerfile Containerfile", "add arg cmd copy entrypoint env expose from healthcheck label maintainer onbuild run shell stopsignal user volume workdir", "#", "", ""),
    (Make, "make", "lang-make", arborium_make, "mk make", "Makefile GNUmakefile", "define endef ifdef ifndef ifeq ifneq else endif include override export unexport private vpath", "#", "", ""),
    (Ini, "ini", "lang-ini", arborium_ini, "ini cfg conf", "", "true false yes no on off", ";", "", ""),
    (Diff, "diff", "lang-diff", arborium_diff, "diff patch", "", "diff index new deleted mode similarity rename from to", "", "", ""),
    (Batch, "batch", "lang-batch", arborium_batch, "bat cmd", "", "assoc break call cd chdir cls color copy date del dir do echo else endlocal equ erase errorlevel exist exit for ftype geq goto gtr if in leq lss md mkdir move neq not path pause popd prompt pushd rd rem ren rename rmdir set setlocal shift start time title type ver verify vol", "rem", "", ""),
    (PowerShell, "powershell", "lang-powershell", arborium_powershell, "ps1 psm1 psd1", "", "begin break catch class clean continue data define do dynamicparam else elseif end enum exit filter finally for foreach from function if in inlinescript parallel param process return switch throw trap try until using var while workflow", "#", "<#", "#>"),
    (Haskell, "haskell", "lang-haskell", arborium_haskell, "hs lhs", "", "as case class data default deriving do else family forall foreign hiding if import in infix infixl infixr instance let mdo module newtype of qualified role then type where", "--", "{-", "-}"),
    (Scala, "scala", "lang-scala", arborium_scala, "scala sc", "", "abstract case catch class def do else enum export extends false final finally for forSome given if implicit import inline lazy match new null object opaque open override package private protected return sealed super then this throw trait transparent true try type using val var while with yield", "//", "/*", "*/"),
}

/// Filename and extension win over a shebang. Only the first 512 bytes are inspected.
pub fn detect(path: impl AsRef<std::path::Path>, first_line: &[u8]) -> Option<Language> {
    let name = path.as_ref().file_name()?.to_str()?.to_ascii_lowercase();
    for &language in Language::ALL {
        if language.filenames().split_whitespace().any(|n| n.eq_ignore_ascii_case(&name)) {
            return Some(language);
        }
    }
    if name.starts_with("dockerfile.") {
        return Some(Language::Dockerfile);
    }
    if let Some((_, ext)) = name.rsplit_once('.') {
        for &language in Language::ALL {
            if language.extensions().split_whitespace().any(|e| e == ext) {
                return Some(language);
            }
        }
    }
    let line = String::from_utf8_lossy(&first_line[..first_line.len().min(512)]);
    let line = line.trim_start_matches('\u{feff}').trim_start();
    if let Some(shebang) = line.strip_prefix("#!") {
        for token in shebang.split_whitespace() {
            let token = token.rsplit('/').next().unwrap_or(token);
            let language = match token {
                t if t.starts_with("python") => Language::Python,
                t if t.starts_with("ruby") => Language::Ruby,
                "sh" | "bash" | "zsh" | "dash" => Language::Bash,
                "node" | "nodejs" | "deno" => Language::JavaScript,
                "pwsh" | "powershell" => Language::PowerShell,
                "perl" => continue,
                "lua" | "luajit" => Language::Lua,
                "php" => Language::Php,
                "runhaskell" | "runghc" => Language::Haskell,
                _ => continue,
            };
            return Some(language);
        }
    }
    if line.starts_with("<?xml") {
        Some(Language::Xml)
    } else if line.to_ascii_lowercase().starts_with("<!doctype html") {
        Some(Language::Html)
    } else if line.starts_with("diff --git ") {
        Some(Language::Diff)
    } else {
        None
    }
}
