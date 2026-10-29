//! The fixed document worker owns detection, formatter processes and the edit.
use super::*;
use kwil_core_commands::settings::Formatter;
use kwil_core_format::{ExternalFormatter, Language, Settings as FormatSettings};

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn builtin(language: &str) -> Language {
    match language {
        "json" => Language::Json,
        "xml" => Language::Xml,
        "html" => Language::Html,
        "yaml" => Language::Yaml,
        "toml" => Language::Toml,
        "css" => Language::Css,
        "sql" => Language::Sql,
        _ => Language::Generic,
    }
}

fn default_formatter(language: &str) -> Option<Formatter> {
    let (executable, args, range_args): (&str, &[&str], &[&str]) = match language {
        "rust" => ("rustfmt", &["--emit", "stdout"], &[]),
        "python" => {
            ("black", &["--quiet", "--stdin-filename", "{file}", "-"], &["--line-ranges", "{start_line}-{end_line}"])
        }
        "javascript" | "typescript" | "tsx" | "jsx" | "markdown" => (
            "prettier",
            &["--stdin-filepath", "{file}"],
            &["--range-start", "{start_utf16}", "--range-end", "{end_utf16}"],
        ),
        _ => return None,
    };
    Some(Formatter {
        executable: executable.into(),
        args: args.iter().map(|s| (*s).into()).collect(),
        range_args: range_args.iter().map(|s| (*s).into()).collect(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests;

fn selected_lines(text: &str, selection: Range<u64>) -> io::Result<Range<usize>> {
    let start = usize::try_from(selection.start).map_err(|_| error("Selection offset exceeds address space"))?;
    let end = usize::try_from(selection.end).map_err(|_| error("Selection offset exceeds address space"))?;
    if start == end {
        return Err(error("Select the lines to format first"));
    }
    if text.get(start..end).is_none() {
        return Err(error("Selection is not on UTF-8 boundaries"));
    }
    let start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    let end = if text.as_bytes()[end - 1] == b'\n' {
        end
    } else {
        text[end..].find('\n').map_or(text.len(), |at| end + at + 1)
    };
    Ok(start..end)
}

impl Editor {
    pub(crate) fn apply_formatting(&mut self, selection: bool, language: Option<String>) -> io::Result<Damage> {
        self.admit_worker_loan(self.document.revision())?;
        let snapshot = self.document.snapshot();
        let settings = FormatSettings {
            indent: if self.settings.editor.insert_spaces {
                kwil_core_format::Indent::Spaces(self.settings.editor.tab_size as usize)
            } else {
                kwil_core_format::Indent::Tabs { width: self.settings.editor.tab_size as usize }
            },
            ..FormatSettings::default()
        };
        // Admit before materializing; never collect an arbitrarily large file.
        let size = usize::try_from(snapshot.len()).map_err(|_| error("Document exceeds formatting limit"))?;
        if size > settings.max_input_bytes {
            return Err(error("Document exceeds the 16 MiB formatter input limit; formatting was not started"));
        }
        kwil_core_format::check_size(size, &settings).map_err(|e| error(e.to_string()))?;
        let language = match language.or_else(|| self.language.clone()) {
            Some(language) => language,
            None => {
                let source = crate::highlighting::SnapshotTextSource::new(snapshot.clone());
                kwil_core_syntax::detection::Detector::new(&[])
                    .and_then(|detector| {
                        detector.detect(
                            self.document.path().unwrap_or(std::path::Path::new("untitled")),
                            None,
                            &source,
                            &|| false,
                        )
                    })
                    .map_err(|e| error(e.to_string()))?
                    .language
                    .id()
                    .to_owned()
            }
        };
        let mut input = Vec::with_capacity(size);
        snapshot.write_to(&mut input)?;
        let text = std::str::from_utf8(&input).map_err(|_| error("Formatting requires valid UTF-8 text"))?;
        let range = if selection {
            if self.cursors.len() != 1 {
                return Err(error("Format Selection requires one selection"));
            }
            Some(selected_lines(text, self.primary_cursor().selection())?)
        } else {
            None
        };
        let config = self.settings.formatters.get(&language).cloned().or_else(|| default_formatter(&language));
        let (replacement, output) = if let Some(config) = config {
            let setting = format!("formatters.{language}");
            if selection && config.range_args.is_empty() {
                return Err(error(format!(
                    "{language} formatter does not support ranges; configure {setting}.range_args or use Format Document"
                )));
            }
            let selected = range.clone().unwrap_or(0..text.len());
            let path = self.document.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| {
                let extension = match language.as_str() {
                    "javascript" => "js",
                    "typescript" => "ts",
                    "markdown" => "md",
                    other => other,
                };
                format!("untitled.{extension}")
            });
            let substitutions = [
                ("{file}", path),
                ("{start_byte}", selected.start.to_string()),
                ("{end_byte}", selected.end.to_string()),
                ("{start_utf16}", text[..selected.start].encode_utf16().count().to_string()),
                ("{end_utf16}", text[..selected.end].encode_utf16().count().to_string()),
                ("{start_line}", (text[..selected.start].bytes().filter(|b| *b == b'\n').count() + 1).to_string()),
                (
                    "{end_line}",
                    (input[..selected.end.saturating_sub(1)].iter().filter(|b| **b == b'\n').count() + 1).to_string(),
                ),
            ];
            let args = config
                .args
                .iter()
                .chain(if selection { config.range_args.iter() } else { [].iter() })
                .map(|arg| substitutions.iter().fold(arg.clone(), |arg, (key, value)| arg.replace(key, value)))
                .collect();
            let command = ExternalFormatter {
                command: config.executable.clone(),
                args,
                timeout: Duration::from_millis(config.timeout_ms as u64),
            };
            let output = kwil_core_format::run_external(text, &command, settings.max_output_bytes).map_err(|e| {
                error(format!(
                    "Check {setting}.executable, {setting}.args and {setting}.timeout_ms: {}: {e}",
                    config.executable
                ))
            })?;
            // Range tools can expand statements. Refuse that expansion rather
            // than changing unselected lines or silently dropping their edits.
            let bytes = output.into_bytes();
            let suffix = &input[selected.end..];
            if bytes.len() < selected.start + suffix.len()
                || !bytes.starts_with(&input[..selected.start])
                || !bytes.ends_with(suffix)
            {
                return Err(error(
                    "Formatter changed lines outside the selection; select the entire statement or use Format Document",
                ));
            }
            let end = bytes.len() - suffix.len();
            (selected.clone(), bytes[selected.start..end].to_vec())
        } else {
            let result = kwil_core_format::format(&input, builtin(&language), &language, range, &settings)
                .map_err(|e| error(e.to_string()))?;
            if result.replacement.is_none() && !result.notices.is_empty() {
                return Err(error(format!(
                    "Configure formatters.{language}.executable and formatters.{language}.args. {}",
                    result.notices.join(" ")
                )));
            }
            let Some(replacement) = result.replacement else {
                return Ok(Damage::default());
            };
            (replacement, result.bytes)
        };
        if replacement.is_empty() && output.is_empty() || input.get(replacement.clone()) == Some(output.as_slice()) {
            return Ok(Damage::default());
        }
        self.replace_byte_range(replacement.start as u64..replacement.end as u64, &output)
    }
}
